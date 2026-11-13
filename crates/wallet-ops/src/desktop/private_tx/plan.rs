use super::*;

pub(in crate::desktop) async fn prepare_desktop_unshield_plan_without_broadcaster_fee(
    request: DesktopUnshieldPlanRequest<'_>,
    http: &HttpContext,
) -> Result<PreparedDesktopUnshieldPlan> {
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
    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain)?;
    let executor = validate_desktop_executor_preparation(
        request.session,
        request.executor.map(|(prepared, _)| prepared),
    )?;
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }

    let artifact_source = artifact_source(http, request.session.db.as_ref())?;
    let prover = ProverService::new_with_db(&artifact_source, &request.session.db);
    let chain_handle = request
        .session
        .sync_manager
        .chain_handle(&request.session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {}", request.chain_id))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();

    let utxos = if let Some((prepared, _)) = request.executor {
        request.session.unspent_utxos_for_executor(prepared)?
    } else {
        request.session.unspent_utxos()
    };
    let receiver_amount = unshield_receiver_amount_for_fee_mode(request.amount, request.fee_mode)?;
    let unshield_request = RailgunUnshieldRequest {
        token_address: request.token,
        amount: receiver_amount,
        recipient: request.recipient,
        verify_proof: request.verify_proof,
        spend_up_to: false,
        broadcaster_fee: None,
        min_gas_price: 0,
    };
    update_transaction_generation_stage(
        request.progress_tx,
        TransactionGenerationStage::SelectingPrivateNotes,
    );
    let selection_info = unshield_selection_info(&utxos, request.token, receiver_amount, false)
        .wrap_err("select POI-verified unshield notes")?;
    let native_top_up = request
        .native_top_up
        .as_ref()
        .map(|_| desktop_native_top_up_plan_from_request(&request, &chain, receiver_amount, &utxos))
        .transpose()?;

    let signer = request.spend_authorization.signer(
        request.vault_store,
        request.view_session,
        "unshield",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
    };

    update_transaction_generation_stage(
        request.progress_tx,
        TransactionGenerationStage::ProvingTransaction,
    );
    if let Some(composite_request) = desktop_composite_unshield_request(
        request.token,
        receiver_amount,
        request.recipient,
        request.unwrap,
        request.verify_proof,
        native_top_up.as_ref(),
        executor,
    )? {
        let plan = tx_builder
            .build_composite_unshield_plan_with_signer(
                &request.view_session.scan_keys(),
                &signer,
                &forest,
                &utxos,
                composite_request,
                &prover,
            )
            .await
            .wrap_err("build desktop composite unshield calldata")?;
        let plan = DesktopUnshieldPreparedPlan::Composite(plan);
        let transaction = issue_desktop_executor_plan(
            request.session,
            request.executor,
            request.spend_authorization,
            &plan,
        )
        .await?;
        return Ok(PreparedDesktopUnshieldPlan {
            transaction,
            plan,
            max_spendable: selection_info.max_spendable,
            prover,
            native_top_up,
        });
    }

    let plan = tx_builder
        .build_unshield_plan_with_signer(
            &request.view_session.scan_keys(),
            &signer,
            &forest,
            &utxos,
            unshield_request,
            &prover,
        )
        .await
        .wrap_err("build desktop unshield calldata")?;

    Ok(PreparedDesktopUnshieldPlan {
        transaction: None,
        plan: DesktopUnshieldPreparedPlan::Single(plan),
        max_spendable: selection_info.max_spendable,
        prover,
        native_top_up: None,
    })
}

pub(in crate::desktop) fn validate_desktop_executor_preparation(
    session: &WalletSession,
    prepared: Option<&PreparedExecutorOperation>,
) -> Result<Option<railgun_wallet::tx::ExecutorContext>> {
    prepared
        .map(|prepared| {
            let owner = session
                .executor_owner()
                .ok_or_else(|| eyre!("executor wallet ownership is unavailable"))?;
            owner.validate_preparation(prepared)?;
            Ok(prepared.context())
        })
        .transpose()
}

/// Issues `prepared`'s execute, its account reads going over the operation's client.
pub(in crate::desktop) async fn issue_desktop_executor_plan(
    session: &WalletSession,
    prepared: Option<(&PreparedExecutorOperation, &OperationHttpClient)>,
    authorization: &DesktopPrivateSpendAuthorization,
    plan: &DesktopUnshieldPreparedPlan,
) -> Result<Option<TransactionRequest>> {
    let Some((prepared, client)) = prepared else {
        return Ok(None);
    };
    let owner = session
        .executor_owner()
        .ok_or_else(|| eyre!("executor wallet ownership is unavailable"))?;
    let issued = owner
        .issue_operation(
            prepared,
            &TransactionCall {
                to: plan.call_to(),
                data: plan.call_data(),
            },
            &plan.input_utxos(),
            authorization,
            client,
        )
        .await?;
    Ok(Some(issued.transaction().clone()))
}

/// Outside private sponsorship, unwrap and native top-up have no legacy `RelayAdapt` route.
pub(crate) fn require_unshield_executor(
    unwrap: bool,
    native_top_up: bool,
    has_executor: bool,
) -> Result<()> {
    if (unwrap || native_top_up) && !has_executor {
        return Err(eyre!(
            "unwrap-to-native and native top-up need the executor route"
        ));
    }
    Ok(())
}

pub(crate) fn desktop_composite_unshield_request(
    token: Address,
    receiver_amount: U256,
    recipient: Address,
    unwrap: bool,
    verify_proof: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    executor: Option<railgun_wallet::tx::ExecutorContext>,
) -> Result<Option<CompositeUnshieldRequest>> {
    require_unshield_executor(unwrap, native_top_up.is_some(), executor.is_some())?;
    let Some(executor) = executor else {
        return Ok(None);
    };
    if let Some(top_up) = native_top_up {
        let mut request = native_top_up_composite_unshield_request(
            token,
            receiver_amount,
            recipient,
            unwrap,
            verify_proof,
            top_up,
        )?;
        request.adapter = Some(CompositeAdapter::Executor(executor));
        return Ok(Some(request));
    }
    if !unwrap {
        return Err(eyre!("this direct unshield does not require an executor"));
    }
    let net = native_top_up_net_after_protocol_fee(receiver_amount);
    Ok(Some(CompositeUnshieldRequest {
        adapter: Some(CompositeAdapter::Executor(executor)),
        legs: vec![CompositeUnshieldLeg {
            token_address: token,
            amount: receiver_amount,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::Primary,
        }],
        relay_actions: Some(CompositeRelayActions {
            min_gas_limit: U256::ZERO,
            calls: vec![
                CompositeRelayAction::UnwrapBase { amount: net },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient,
                    amount: net,
                },
            ],
        }),
        broadcaster_fee: None,
        min_gas_price: 0,
        verify_proof,
        spend_up_to: false,
    }))
}

fn desktop_native_top_up_plan_from_request(
    request: &DesktopUnshieldPlanRequest<'_>,
    chain: &EffectiveDesktopChainConfig,
    receiver_amount: U256,
    utxos: &[Utxo],
) -> Result<DesktopNativeTopUpPlan> {
    desktop_native_top_up_plan_from_unshield_fields(
        request.chain_id,
        chain,
        request.token,
        request.recipient,
        request.unwrap,
        receiver_amount,
        None,
        U256::ZERO,
        utxos,
    )
}

pub(in crate::desktop) fn desktop_native_top_up_plan_from_unshield_fields(
    chain_id: u64,
    chain: &EffectiveDesktopChainConfig,
    token: Address,
    recipient: Address,
    unwrap: bool,
    receiver_amount: U256,
    broadcaster_fee_token: Option<Address>,
    broadcaster_fee_amount: U256,
    utxos: &[Utxo],
) -> Result<DesktopNativeTopUpPlan> {
    let policy = native_top_up_policy_for_chain(chain_id)
        .ok_or_else(|| eyre!("selected chain does not support native top-up"))?;
    let wrapped_native_token = chain
        .wrapped_native_token
        .ok_or_else(|| eyre!("selected chain has no wrapped native token for native top-up"))?;
    if unwrap {
        return Err(eyre!(
            "native top-up cannot be combined with unwrap-to-native output"
        ));
    }
    let wrapped_native_amount = native_top_up_wrapped_native_amount(policy.top_up_amount);
    let mut required_wrapped_native = native_top_up_required_wrapped_native_amount(
        token,
        wrapped_native_token,
        receiver_amount,
        policy.top_up_amount,
    );
    if broadcaster_fee_token == Some(wrapped_native_token) {
        required_wrapped_native = required_wrapped_native.saturating_add(broadcaster_fee_amount);
    }
    let max_wrapped_native = max_unshield_spendable(utxos, wrapped_native_token);
    if max_wrapped_native < required_wrapped_native {
        return Err(eyre!(
            "native top-up wrapped-native max spendable: {max_wrapped_native}; required: {required_wrapped_native}"
        ));
    }

    Ok(DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token,
        native_amount: policy.top_up_amount,
        wrapped_native_amount,
    })
}

pub(in crate::desktop) fn desktop_native_top_up_plan_for_estimate(
    chain_id: u64,
    chain: &EffectiveDesktopChainConfig,
    _token: Address,
    recipient: Address,
    unwrap: bool,
    _receiver_amount: U256,
) -> Result<DesktopNativeTopUpPlan> {
    let policy = native_top_up_policy_for_chain(chain_id)
        .ok_or_else(|| eyre!("selected chain does not support native top-up"))?;
    let wrapped_native_token = chain
        .wrapped_native_token
        .ok_or_else(|| eyre!("selected chain has no wrapped native token for native top-up"))?;
    if unwrap {
        return Err(eyre!(
            "native top-up cannot be combined with unwrap-to-native output"
        ));
    }
    let wrapped_native_amount = native_top_up_wrapped_native_amount(policy.top_up_amount);

    Ok(DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token,
        native_amount: policy.top_up_amount,
        wrapped_native_amount,
    })
}

pub(crate) fn native_top_up_composite_unshield_request(
    token: Address,
    receiver_amount: U256,
    recipient: Address,
    unwrap: bool,
    verify_proof: bool,
    native_top_up: &DesktopNativeTopUpPlan,
) -> Result<CompositeUnshieldRequest> {
    if unwrap {
        return Err(eyre!(
            "native top-up cannot be combined with unwrap-to-native output"
        ));
    }

    let wrapped_native = native_top_up.wrapped_native_token;
    let native_amount = native_top_up.native_amount;
    let wrapped_native_amount = native_top_up.wrapped_native_amount;
    let mut calls = vec![
        CompositeRelayAction::UnwrapBase {
            amount: native_amount,
        },
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::BaseNative,
            recipient,
            amount: native_amount,
        },
    ];
    let legs = if token == wrapped_native {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            token,
            wrapped_native,
            receiver_amount,
            native_amount,
        );
        let wrapped_output_amount =
            native_top_up_net_after_protocol_fee(combined_wrapped_native_amount) - native_amount;
        calls.push(CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::Erc20(wrapped_native),
            recipient,
            amount: wrapped_output_amount,
        });
        vec![CompositeUnshieldLeg {
            token_address: wrapped_native,
            amount: combined_wrapped_native_amount,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::WrappedNativeOutput,
        }]
    } else {
        vec![
            CompositeUnshieldLeg {
                token_address: token,
                amount: receiver_amount,
                recipient: CompositeUnshieldRecipient::Public(recipient),
                role: CompositeUnshieldLegRole::Primary,
            },
            CompositeUnshieldLeg {
                token_address: wrapped_native,
                amount: wrapped_native_amount,
                recipient: CompositeUnshieldRecipient::RelayAdapt,
                role: CompositeUnshieldLegRole::NativeTopUp,
            },
        ]
    };

    Ok(CompositeUnshieldRequest {
        adapter: None,
        legs,
        relay_actions: Some(CompositeRelayActions {
            min_gas_limit: U256::ZERO,
            calls,
        }),
        broadcaster_fee: None,
        min_gas_price: 0,
        verify_proof,
        spend_up_to: false,
    })
}

pub async fn prepare_blocked_shield_rescue_preview(
    request: BlockedShieldRescuePreviewRequest,
    http: &HttpContext,
) -> Result<BlockedShieldRescuePreview> {
    let utxo = selected_blocked_shield_rescue_utxo(&request.session, &request.utxo_id)?;
    let eligibility = resolve_blocked_shield_rescue_eligibility(
        BlockedShieldRescueEligibilityRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain,
            view_session: request.view_session,
            session: request.session,
            vault_store: request.vault_store,
            utxo_id: request.utxo_id,
        },
        http,
        &http.operation_http_client().await?,
    )
    .await?;
    let origin_address = eligibility
        .origin_address
        .ok_or_else(|| eyre!("blocked Shield refund origin is unresolved"))?;
    let public_account_uuid = eligibility
        .public_account_uuid
        .ok_or_else(|| eyre!("blocked Shield refund origin Public account is unavailable"))?;
    if !eligibility.eligible {
        return Err(eyre!(
            "blocked Shield refund is unavailable: {}",
            eligibility
                .disabled_reason
                .as_deref()
                .unwrap_or("eligibility check failed")
        ));
    }

    Ok(BlockedShieldRescuePreview {
        chain_id: request.chain_id,
        utxo_id: request.utxo_id,
        token: utxo.token_address(),
        amount: utxo.note.value,
        source_tx_hash: utxo.source.tx_hash,
        origin_address,
        public_account_uuid,
        public_account_label: eligibility.public_account_label,
    })
}

pub(in crate::desktop) async fn prepare_blocked_shield_rescue_plan(
    request: &BlockedShieldRescueSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PreparedBlockedShieldRescuePlan> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    let utxo = selected_blocked_shield_rescue_utxo(&request.session, &request.utxo_id)?;
    let token = utxo.token_address();
    let amount = utxo.note.value;
    let eligibility = resolve_blocked_shield_rescue_eligibility(
        BlockedShieldRescueEligibilityRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.clone(),
            view_session: Arc::clone(&request.view_session),
            session: Arc::clone(&request.session),
            vault_store: Arc::clone(&request.vault_store),
            utxo_id: request.utxo_id,
        },
        http,
        client,
    )
    .await?;
    if !eligibility.eligible {
        return Err(eyre!(
            "blocked Shield refund is unavailable: {}",
            eligibility
                .disabled_reason
                .as_deref()
                .unwrap_or("eligibility check failed")
        ));
    }
    let origin_address = eligibility
        .origin_address
        .ok_or_else(|| eyre!("blocked Shield refund origin is unresolved"))?;
    let public_account_uuid = matched_blocked_shield_rescue_public_account_uuid(
        eligibility.public_account_uuid.as_deref(),
        request.requested_public_account_uuid.as_deref(),
    )?;

    let artifact_source = artifact_source(http, request.session.db.as_ref())?;
    let prover = ProverService::new_with_db(&artifact_source, &request.session.db);
    let chain_handle = request
        .session
        .sync_manager
        .chain_handle(&request.session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {}", request.chain_id))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();

    let rescue_utxos = vec![utxo.clone()];
    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::SelectingPrivateNotes,
    );
    let selection_info = unshield_selection_info(&rescue_utxos, token, amount, false)
        .wrap_err("select blocked Shield refund note")?;
    if selection_info.input_count != 1 || selection_info.max_spendable != amount {
        return Err(eyre!(
            "blocked Shield refund must select exactly the chosen UTXO"
        ));
    }

    let signer = request.spend_authorization.signer(
        request.vault_store.as_ref(),
        request.view_session.as_ref(),
        "blocked Shield refund",
    )?;
    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
    };
    let unshield_request = RailgunUnshieldRequest {
        token_address: token,
        amount,
        recipient: origin_address,
        verify_proof: request.verify_proof,
        spend_up_to: false,
        broadcaster_fee: None,
        min_gas_price: 0,
    };

    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::ProvingTransaction,
    );
    let plan = tx_builder
        .build_unshield_plan_with_signer(
            &request.view_session.scan_keys(),
            &signer,
            &forest,
            &rescue_utxos,
            unshield_request,
            &prover,
        )
        .await
        .wrap_err("build blocked Shield refund calldata")?;
    validate_blocked_shield_rescue_plan(&plan, &request.utxo_id, token, amount, origin_address)?;

    Ok(PreparedBlockedShieldRescuePlan {
        plan,
        public_account_uuid,
        token,
        origin_address,
    })
}

pub(in crate::desktop) fn selected_blocked_shield_rescue_utxo(
    session: &WalletSession,
    utxo_id: &BlockedShieldRescueUtxoId,
) -> Result<Utxo> {
    let snapshot = session
        .handle
        .current_snapshot()
        .ok_or_else(|| eyre!("wallet state is unavailable while synchronization resets"))?;
    blocked_shield_rescue_candidate_from_records(
        &snapshot.utxos,
        &snapshot.pending_overlay,
        utxo_id,
    )
    .ok_or_else(|| eyre!("selected UTXO is not an unspent blocked Shield that can be refunded"))
}

pub(crate) fn matched_blocked_shield_rescue_public_account_uuid(
    matched: Option<&str>,
    requested: Option<&str>,
) -> Result<String> {
    let matched =
        matched.ok_or_else(|| eyre!("blocked Shield refund origin account is unavailable"))?;
    if let Some(requested) = requested
        && requested != matched
    {
        return Err(eyre!(
            "blocked Shield refund gas payer must be the matched origin Public account"
        ));
    }
    Ok(matched.to_string())
}

pub(crate) fn validate_blocked_shield_rescue_plan(
    plan: &UnshieldPlan,
    utxo_id: &BlockedShieldRescueUtxoId,
    token: Address,
    amount: U256,
    origin_address: Address,
) -> Result<()> {
    if plan.inputs.len() != 1 {
        return Err(eyre!(
            "blocked Shield refund must spend exactly one private input"
        ));
    }
    let input = &plan.inputs[0].utxo;
    if !blocked_shield_rescue_utxo_matches(input, utxo_id) {
        return Err(eyre!("blocked Shield refund selected an unexpected UTXO"));
    }
    if input.note.value != amount || plan.unshield_note.value != amount {
        return Err(eyre!(
            "blocked Shield refund must spend the full UTXO value"
        ));
    }
    let expected_unshield = Note::new_unshield(origin_address, token, amount);
    if plan.unshield_note.token_hash != expected_unshield.token_hash
        || plan.unshield_note.npk != expected_unshield.npk
    {
        return Err(eyre!(
            "blocked Shield refund must unshield the exact token to the origin address"
        ));
    }
    if plan.unshield_notes.len() != 1 {
        return Err(eyre!(
            "blocked Shield refund must have exactly one public output"
        ));
    }
    if plan.broadcaster_fee_note.is_some() {
        return Err(eyre!(
            "blocked Shield refund cannot include a broadcaster fee note"
        ));
    }
    if plan.change_note.is_some() {
        return Err(eyre!("blocked Shield refund cannot create private change"));
    }
    for chunk in &plan.chunks {
        if chunk.private_output_count() != Some(0) {
            return Err(eyre!("blocked Shield refund cannot create private outputs"));
        }
    }
    Ok(())
}

pub(in crate::desktop) async fn prepare_desktop_send_plan_without_broadcaster_fee(
    request: DesktopSendPlanRequest<'_>,
    http: &HttpContext,
) -> Result<PreparedPrivatePlan<SendPlan>> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }

    let recipient = request.recipient.trim();
    let recipient_data = parse_railgun_recipient(recipient)?;
    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain)?;
    let artifact_source = artifact_source(http, request.session.db.as_ref())?;
    let prover = ProverService::new_with_db(&artifact_source, &request.session.db);
    let chain_handle = request
        .session
        .sync_manager
        .chain_handle(&request.session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {}", request.chain_id))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();

    let utxos = request.session.unspent_utxos();
    let send_request = RailgunSendRequest {
        token_address: request.token,
        amount: request.amount,
        recipient: recipient_data,
        verify_proof: request.verify_proof,
        spend_up_to: false,
        broadcaster_fee: None,
        min_gas_price: 0,
    };
    update_transaction_generation_stage(
        request.progress_tx,
        TransactionGenerationStage::SelectingPrivateNotes,
    );
    let selection_info = send_selection_info(&utxos, request.token, request.amount, false)
        .wrap_err("select POI-verified send notes")?;

    let signer =
        request
            .spend_authorization
            .signer(request.vault_store, request.view_session, "send")?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
    };

    update_transaction_generation_stage(
        request.progress_tx,
        TransactionGenerationStage::ProvingTransaction,
    );
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
        .wrap_err("build desktop send calldata")?;

    Ok(PreparedPrivatePlan {
        plan,
        max_spendable: selection_info.max_spendable,
        prover,
    })
}

pub(in crate::desktop) async fn persist_manual_unshield_pending_pois(
    plan: &DesktopUnshieldPreparedPlan,
    session: &WalletSession,
    chain_id: u64,
    _wallet_id: &str,
    prover: &ProverService,
    verify_proof: bool,
    http: &HttpContext,
    operation_label: &'static str,
) -> Result<()> {
    let (pending_poi_list_keys, pending_pois) = active_list_pre_transaction_pois(
        plan.chunks(),
        session,
        chain_id,
        prover,
        verify_proof,
        http,
        operation_label,
    )
    .await?;
    match plan {
        DesktopUnshieldPreparedPlan::Single(plan) => {
            persist_pending_unshield_output_poi_contexts(
                session,
                &plan.chunks,
                &pending_pois,
                &pending_poi_list_keys,
                false,
                false,
            )
            .await?;
        }
        DesktopUnshieldPreparedPlan::Composite(plan) => {
            persist_pending_composite_unshield_output_poi_contexts(
                session,
                &plan.chunks,
                &plan.private_output_roles,
                &pending_pois,
                &pending_poi_list_keys,
            )
            .await?;
        }
    }
    Ok(())
}

pub(in crate::desktop) async fn persist_manual_send_pending_pois(
    plan: &SendPlan,
    session: &WalletSession,
    chain_id: u64,
    _wallet_id: &str,
    prover: &ProverService,
    verify_proof: bool,
    http: &HttpContext,
    operation_label: &'static str,
) -> Result<()> {
    let (pending_poi_list_keys, pending_pois) = active_list_pre_transaction_pois(
        &plan.chunks,
        session,
        chain_id,
        prover,
        verify_proof,
        http,
        operation_label,
    )
    .await?;
    persist_pending_send_output_poi_contexts(
        session,
        &plan.chunks,
        &pending_pois,
        &pending_poi_list_keys,
        false,
        false,
    )
    .await?;
    Ok(())
}

pub(crate) fn unshield_chunks_require_pending_output_pois(chunks: &[TransactionPlanChunk]) -> bool {
    chunks
        .iter()
        .any(|chunk| chunk.private_output_count().is_none_or(|count| count > 0))
}

pub(in crate::desktop) fn prepared_unshield_call_from_plan(
    chain_id: u64,
    token: Address,
    amount: U256,
    fee_mode: FeeHandlingMode,
    recipient: Address,
    unwrap: bool,
    max_spendable: U256,
    plan: &DesktopUnshieldPreparedPlan,
    native_top_up: Option<DesktopNativeTopUpPlan>,
) -> PreparedUnshieldCall {
    PreparedUnshieldCall {
        chain_id,
        token,
        amount,
        fee_mode,
        recipient,
        unwrap,
        max_spendable,
        transaction_count: plan.transaction_count(),
        input_count: plan.input_count(),
        private_output_count: plan.private_output_count(),
        public_output_count: plan.public_output_count(),
        to: plan.call_to(),
        data: hex::encode_prefixed(plan.call_data()),
        native_top_up,
    }
}

pub(in crate::desktop) fn prepared_send_call_from_plan(
    chain_id: u64,
    token: Address,
    amount: U256,
    recipient: String,
    max_spendable: U256,
    plan: &SendPlan,
) -> PreparedSendCall {
    PreparedSendCall {
        chain_id,
        token,
        amount,
        recipient,
        max_spendable,
        transaction_count: plan.transaction_count(),
        input_count: plan.input_count(),
        private_output_count: plan.private_output_count(),
        public_output_count: plan.public_output_count(),
        to: plan.call.to,
        data: hex::encode_prefixed(&plan.call.data),
    }
}
