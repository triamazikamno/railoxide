use super::*;
use eyre::eyre;

pub(super) async fn prepare_desktop_unshield_plan_without_broadcaster_fee(
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
    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain)?;
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

    let utxos = request.session.unspent_utxos();
    let mode = if request.unwrap {
        UnshieldMode::UnwrapBase
    } else {
        UnshieldMode::Token
    };
    let receiver_amount = unshield_receiver_amount_for_fee_mode(request.amount, request.fee_mode)?;
    let unshield_request = RailgunUnshieldRequest {
        token_address: request.token,
        amount: receiver_amount,
        recipient: request.recipient,
        mode,
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

    let signer = request.spend_authorization.into_signer(
        request.vault_store,
        request.view_session,
        "unshield",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
        relay_adapt_contract: chain.relay_adapt_contract,
    };

    update_transaction_generation_stage(
        request.progress_tx,
        TransactionGenerationStage::ProvingTransaction,
    );
    if let Some(native_top_up) = native_top_up {
        let composite_request = native_top_up_composite_unshield_request(
            request.token,
            receiver_amount,
            request.recipient,
            request.unwrap,
            request.verify_proof,
            &native_top_up,
        )?;
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

        return Ok(PreparedDesktopUnshieldPlan {
            plan: DesktopUnshieldPreparedPlan::Composite(plan),
            max_spendable: selection_info.max_spendable,
            prover,
            native_top_up: Some(native_top_up),
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
        plan: DesktopUnshieldPreparedPlan::Single(plan),
        max_spendable: selection_info.max_spendable,
        prover,
        native_top_up: None,
    })
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

pub(super) fn desktop_native_top_up_plan_from_unshield_fields(
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

pub(super) fn desktop_native_top_up_plan_for_estimate(
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

pub(super) async fn prepare_blocked_shield_rescue_plan(
    request: &BlockedShieldRescueSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<PreparedBlockedShieldRescuePlan> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain.as_ref())?;
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
        relay_adapt_contract: chain.relay_adapt_contract,
    };
    let unshield_request = RailgunUnshieldRequest {
        token_address: token,
        amount,
        recipient: origin_address,
        mode: UnshieldMode::Token,
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
    })
}

pub(super) fn selected_blocked_shield_rescue_utxo(
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

pub(super) async fn prepare_desktop_send_plan_without_broadcaster_fee(
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

    let signer = request.spend_authorization.into_signer(
        request.vault_store,
        request.view_session,
        "send",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
        relay_adapt_contract: chain.relay_adapt_contract,
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

pub(super) async fn persist_manual_unshield_pending_pois(
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

pub(super) async fn persist_manual_send_pending_pois(
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

pub(super) fn prepared_unshield_call_from_plan(
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

pub(super) fn prepared_send_call_from_plan(
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

pub async fn prepare_desktop_unshield_calldata(
    request: DesktopUnshieldCalldataRequest,
    http: &HttpContext,
) -> Result<PreparedUnshieldCall> {
    let prepared = prepare_desktop_unshield_plan_without_broadcaster_fee(
        DesktopUnshieldPlanRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.as_ref(),
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: request.spend_authorization,
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
            effective_chain: request.effective_chain.as_ref(),
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: request.spend_authorization,
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

#[derive(Clone, Copy)]
enum SponsoredPrivateIntent {
    Send {
        token: Address,
        amount: U256,
        recipient: AddressData,
    },
    Unshield {
        token: Address,
        amount: U256,
        recipient: Address,
        unwrap: bool,
    },
}

impl SponsoredPrivateIntent {
    const fn action(self) -> SponsoredActionKind {
        match self {
            Self::Send { .. } => SponsoredActionKind::Send,
            Self::Unshield { .. } => SponsoredActionKind::Unshield,
        }
    }

    fn mixed_request(
        self,
        authorization: &SponsoredAuthorization,
        native_top_up: Option<&DesktopNativeTopUpPlan>,
        verify_proof: bool,
        rebuild: Option<MixedPrivateActionRebuildConstraint>,
    ) -> std::result::Result<MixedPrivateActionRequest, SponsorshipError> {
        let private_sends = match self {
            Self::Send {
                token,
                amount,
                recipient,
            } => vec![MixedPrivateSend {
                token_address: token,
                amount,
                recipient,
                role: MixedPrivateSendRole::Primary,
            }],
            Self::Unshield { .. } => Vec::new(),
        };
        let mut public_unshields = Vec::new();
        let mut calls = if authorization.builder_payment.is_zero() {
            Vec::new()
        } else {
            vec![
                CompositeRelayAction::UnwrapBase {
                    amount: authorization.builder_payment,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient: authorization.coinbase_payer,
                    amount: authorization.builder_payment,
                },
            ]
        };
        if let Self::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } = self
        {
            if let Some(native_top_up) = native_top_up {
                let top_up = native_top_up_composite_unshield_request(
                    token,
                    amount,
                    recipient,
                    unwrap,
                    false,
                    native_top_up,
                )
                .expect("validated sponsored native top-up");
                public_unshields.extend(top_up.legs);
                calls.extend(
                    top_up
                        .relay_actions
                        .expect("native top-up uses RelayAdapt")
                        .calls,
                );
            } else {
                let route_wrapped_native_output = !authorization.builder_payment.is_zero()
                    && !unwrap
                    && token == authorization.wrapped_native_token;
                public_unshields.push(CompositeUnshieldLeg {
                    token_address: token,
                    amount,
                    recipient: if unwrap || route_wrapped_native_output {
                        CompositeUnshieldRecipient::RelayAdapt
                    } else {
                        CompositeUnshieldRecipient::Public(recipient)
                    },
                    role: CompositeUnshieldLegRole::Primary,
                });
                let recipient_amount = native_top_up_net_after_protocol_fee(amount);
                if unwrap {
                    calls.extend([
                        CompositeRelayAction::UnwrapBase {
                            amount: recipient_amount,
                        },
                        CompositeRelayAction::Transfer {
                            token: CompositeRelayActionToken::BaseNative,
                            recipient,
                            amount: recipient_amount,
                        },
                    ]);
                } else if route_wrapped_native_output {
                    calls.push(CompositeRelayAction::Transfer {
                        token: CompositeRelayActionToken::Erc20(token),
                        recipient,
                        amount: recipient_amount,
                    });
                }
            }
        }
        if !calls.is_empty() {
            coalesce_sponsored_wrapped_native_leg(
                &mut public_unshields,
                &calls,
                authorization.wrapped_native_token,
            )?;
        }
        let relay_actions = (!calls.is_empty()).then_some(CompositeRelayActions {
            min_gas_limit: U256::ZERO,
            calls,
        });
        Ok(MixedPrivateActionRequest {
            private_sends,
            public_unshields,
            relay_actions,
            min_gas_price: 0,
            verify_proof,
            spend_up_to: false,
            rebuild,
        })
    }
}

fn coalesce_sponsored_wrapped_native_leg(
    public_unshields: &mut Vec<CompositeUnshieldLeg>,
    calls: &[CompositeRelayAction],
    wrapped_native: Address,
) -> std::result::Result<U256, SponsorshipError> {
    let net_wrapped_native = calls.iter().try_fold(U256::ZERO, |total, call| {
        let consumed = match call {
            CompositeRelayAction::UnwrapBase { amount } => *amount,
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::Erc20(token),
                amount,
                ..
            } if *token == wrapped_native => *amount,
            CompositeRelayAction::Transfer { .. } => U256::ZERO,
        };
        total
            .checked_add(consumed)
            .ok_or(SponsorshipError::ArithmeticOverflow)
    })?;
    let gross_wrapped_native = gross_up_sponsorship_payment(net_wrapped_native)?;
    if native_top_up_net_after_protocol_fee(gross_wrapped_native) != net_wrapped_native {
        return Err(SponsorshipError::ArithmeticOverflow);
    }
    public_unshields.retain(|leg| {
        leg.token_address != wrapped_native
            || leg.recipient != CompositeUnshieldRecipient::RelayAdapt
    });
    public_unshields.insert(
        0,
        CompositeUnshieldLeg {
            token_address: wrapped_native,
            amount: gross_wrapped_native,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::SponsoredWrappedNative,
        },
    );
    Ok(gross_wrapped_native)
}

fn sponsored_action_fingerprint_bytes(
    chain_id: u64,
    action: SponsoredActionKind,
    token: Address,
    amount: U256,
) -> Vec<u8> {
    let mut bytes = b"railoxide:sponsored-action:v1".to_vec();
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.push(match action {
        SponsoredActionKind::Send => 1,
        SponsoredActionKind::Unshield => 2,
        SponsoredActionKind::BlockedShield => 3,
        SponsoredActionKind::PublicAction => 4,
    });
    bytes.extend_from_slice(token.as_slice());
    bytes.extend_from_slice(&amount.to_be_bytes::<32>());
    bytes
}

#[must_use]
pub fn sponsored_send_action_fingerprint(
    chain_id: u64,
    token: Address,
    amount: U256,
    recipient: &AddressData,
) -> FixedBytes<32> {
    let mut bytes =
        sponsored_action_fingerprint_bytes(chain_id, SponsoredActionKind::Send, token, amount);
    bytes.extend_from_slice(&recipient.master_public_key.to_be_bytes::<32>());
    bytes.extend_from_slice(&recipient.viewing_public_key);
    keccak256(bytes)
}

#[must_use]
pub fn sponsored_unshield_action_fingerprint(
    chain_id: u64,
    token: Address,
    amount: U256,
    recipient: Address,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
) -> FixedBytes<32> {
    let mut bytes =
        sponsored_action_fingerprint_bytes(chain_id, SponsoredActionKind::Unshield, token, amount);
    bytes.extend_from_slice(recipient.as_slice());
    bytes.push(u8::from(unwrap));
    if let Some(top_up) = native_top_up {
        bytes.push(1);
        bytes.extend_from_slice(top_up.recipient.as_slice());
        bytes.extend_from_slice(top_up.wrapped_native_token.as_slice());
        bytes.extend_from_slice(&top_up.native_amount.to_be_bytes::<32>());
        bytes.extend_from_slice(&top_up.wrapped_native_amount.to_be_bytes::<32>());
    } else {
        bytes.push(0);
    }
    keccak256(bytes)
}

fn sponsored_approximate_gas(
    request: &MixedPrivateActionRequest,
    preview: &railgun_wallet::tx::MixedPrivateActionPreview,
    send: bool,
) -> u64 {
    let unwrap_count = request.relay_actions.as_ref().map_or(0, |actions| {
        actions
            .calls
            .iter()
            .filter(|action| matches!(action, CompositeRelayAction::UnwrapBase { .. }))
            .count()
    });
    approximate_public_broadcaster_gas(ApproximateTransactionShape {
        transaction_count: preview.shape.transaction_count,
        input_count: preview.shape.input_count,
        private_output_count: preview.shape.private_output_count,
        public_output_count: preview.shape.public_output_count,
        max_receiver_amount: U256::ZERO,
        relay_call_count: preview.shape.relay_call_count,
        uses_relay_adapt: preview.shape.uses_relay_adapt,
        unwrap_count,
        send,
    })
}

#[allow(clippy::too_many_arguments)]
fn sponsored_provisional_payment_for_intent(
    chain_id: u64,
    chain: &EffectiveDesktopChainConfig,
    utxos: &[Utxo],
    wrapped_native: Address,
    payer: Address,
    signer: Address,
    intent: SponsoredPrivateIntent,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
) -> Result<SponsorshipPayment> {
    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id,
        railgun_contract: chain.railgun_contract,
        relay_adapt_contract: chain.relay_adapt_contract,
    };
    let poi_spendable_wrapped_native = poi_spendable_token_balance(utxos, wrapped_native);
    let preview_gas = |payment: SponsorshipPayment| -> Result<u64> {
        let authorization = sponsored_authorization(
            intent.action(),
            wrapped_native,
            payer,
            chain.relay_adapt_contract,
            payment,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            signer,
        );
        let request = intent.mixed_request(&authorization, native_top_up, false, None)?;
        let public_wrapped_native_spend = request_public_token_spend(&request, wrapped_native)?;
        let required_wrapped_native = sponsored_total_wrapped_native_spend(
            intent,
            public_wrapped_native_spend,
            wrapped_native,
        )?;
        if poi_spendable_wrapped_native < required_wrapped_native {
            return Err(SponsorshipError::InsufficientWrappedNativeForQuote {
                available: poi_spendable_wrapped_native,
            }
            .into());
        }
        let preview = tx_builder.preview_mixed_private_action_plan(utxos, &request)?;
        Ok(sponsored_approximate_gas(
            &request,
            &preview,
            matches!(intent, SponsoredPrivateIntent::Send { .. }),
        ))
    };
    let initial_payment = sponsorship_payment(
        0,
        max_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
    )?;
    let mut estimated_gas = preview_gas(initial_payment)?;
    for _ in 0..SPONSORED_PROVISIONAL_MAX_STEPS {
        let payment = sponsorship_payment_from_estimate(
            estimated_gas,
            chain.gas.gas_limit_buffer,
            max_fee_per_gas,
            signer_native_balance_snapshot,
            incentive,
        )?;
        let next_estimated_gas = preview_gas(payment)?;
        if next_estimated_gas <= estimated_gas {
            return Ok(payment);
        }
        estimated_gas = next_estimated_gas;
    }
    Err(SponsorshipError::ProvisionalPaymentDidNotConverge.into())
}

#[allow(clippy::too_many_arguments)]
fn quote_sponsored_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    intent: SponsoredPrivateIntent,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    if effective_chain.chain_id != chain_id {
        return Err(eyre!(
            "effective chain config is for chain {}, not {chain_id}",
            effective_chain.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(chain_id, Some(effective_chain))?;
    let wrapped_native = chain
        .wrapped_native_token
        .ok_or(SponsorshipError::MissingWrappedNativeToken)?;
    let payer = effective_chain
        .coinbase_payer
        .ok_or(SponsorshipError::MissingCoinbasePayer)?;
    if effective_chain.sponsored_bundle_relays.is_empty() {
        return Err(SponsorshipError::MissingRelay.into());
    }
    let payment = sponsored_provisional_payment_for_intent(
        chain_id,
        &chain,
        utxos,
        wrapped_native,
        payer,
        signer,
        intent,
        native_top_up,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
    )?;
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
            native_top_up,
        ),
    };
    let maximum_authorization = sponsored_authorization(
        intent.action(),
        wrapped_native,
        payer,
        chain.relay_adapt_contract,
        payment,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer,
    );
    let maximum_request =
        intent.mixed_request(&maximum_authorization, native_top_up, false, None)?;
    let public_wrapped_native_spend = maximum_request
        .public_unshields
        .iter()
        .filter(|leg| leg.token_address == wrapped_native)
        .try_fold(U256::ZERO, |total, leg| {
            total
                .checked_add(leg.amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        })?;
    let max_total_wrapped_native_spend =
        sponsored_total_wrapped_native_spend(intent, public_wrapped_native_spend, wrapped_native)?;
    sponsored_authorization_limit(
        action_fingerprint,
        payment.outer_gas_limit,
        intent.action(),
        wrapped_native,
        payer,
        chain.relay_adapt_contract,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
        max_total_wrapped_native_spend,
    )
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
pub fn quote_sponsored_send_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    token: Address,
    amount: U256,
    recipient: &AddressData,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    quote_sponsored_authorization_limit(
        chain_id,
        effective_chain,
        utxos,
        SponsoredPrivateIntent::Send {
            token,
            amount,
            recipient: *recipient,
        },
        None,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn quote_sponsored_unshield_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    recipient: Address,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    let amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    quote_sponsored_authorization_limit(
        chain_id,
        effective_chain,
        utxos,
        SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        },
        native_top_up,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
    )
}

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

fn sponsored_wrapped_native_output_total(
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

fn optional_sponsored_wrapped_native_leg_amount(
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

fn request_public_token_spend(
    request: &MixedPrivateActionRequest,
    token: Address,
) -> std::result::Result<U256, SponsorshipError> {
    request
        .public_unshields
        .iter()
        .filter(|leg| leg.token_address == token)
        .try_fold(U256::ZERO, |total, leg| {
            total
                .checked_add(leg.amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        })
}

fn sponsored_total_wrapped_native_spend(
    intent: SponsoredPrivateIntent,
    public_total: U256,
    wrapped_native: Address,
) -> std::result::Result<U256, SponsorshipError> {
    match intent {
        SponsoredPrivateIntent::Send { token, amount, .. } if token == wrapped_native => {
            public_total
                .checked_add(amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        }
        _ => Ok(public_total),
    }
}

fn sponsored_rebuild_error(error: BuildError) -> Report {
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
    let chain = effective_desktop_chain_config(chain_id, Some(effective_chain))?;
    let wrapped_native = chain
        .wrapped_native_token
        .ok_or(SponsorshipError::MissingWrappedNativeToken)?;
    let payer = effective_chain
        .coinbase_payer
        .ok_or(SponsorshipError::MissingCoinbasePayer)?;
    let signer_address = self_broadcast_gas_payer(vault_store, view_session, public_account_uuid)?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls.clone(), http);

    let provider_handle =
        sponsored_code_preflight_from_rpc_pool(&query_rpc_pool, payer, signer_address).await?;
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
    validate_sponsored_admission(SponsoredAdmission {
        action: intent.action(),
        delivery: PrivateDeliveryMode::SelfBroadcast,
        has_relays: !effective_chain.sponsored_bundle_relays.is_empty(),
        wrapped_native_token: Some(wrapped_native),
        coinbase_payer: Some(payer),
        payer_verified: true,
        signer_eligible: true,
        poi_spendable_wrapped_native,
        required_wrapped_native: authorization_limit.max_total_wrapped_native_spend,
    })?;

    let quote = self_broadcast_gas_fee_quote_from_rpc_pool(&query_rpc_pool, http.network_mode())
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
        || authorization_limit.relay_adapt_contract != chain.relay_adapt_contract
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
        signer_address,
        intent,
        native_top_up.as_ref(),
        resolved_fee.max_fee_per_gas,
        resolved_fee.max_priority_fee_per_gas,
        authorization_limit.signer_native_balance_snapshot,
        incentive,
    )?;
    let provisional_authorization = sponsored_authorization(
        intent.action(),
        wrapped_native,
        payer,
        chain.relay_adapt_contract,
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
        relay_adapt_contract: chain.relay_adapt_contract,
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
                chain.relay_adapt_contract,
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
            (first_plan, provisional_authorization)
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

pub async fn prepare_desktop_sponsored_send_calldata(
    request: DesktopSponsoredSendCalldataRequest,
    http: &HttpContext,
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
    )
    .await
}

pub async fn prepare_desktop_sponsored_unshield_calldata(
    request: DesktopSponsoredUnshieldCalldataRequest,
    http: &HttpContext,
) -> Result<PreparedSponsoredCall> {
    let chain = effective_desktop_chain_config(request.chain_id, Some(&request.effective_chain))?;
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
    )
    .await
}

pub async fn submit_prepared_desktop_sponsored_self_broadcast(
    request: DesktopPreparedSponsoredSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<SponsoredSelfBroadcastSessionOutcome> {
    submit_prepared_sponsored_self_broadcast(request, http).await
}

pub async fn submit_desktop_sponsored_send_self_broadcast(
    request: DesktopSponsoredSendSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<DesktopSponsoredSelfBroadcastResult> {
    let prepared = prepare_desktop_sponsored_send_calldata(
        DesktopSponsoredSendCalldataRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.clone(),
            view_session: Arc::clone(&request.view_session),
            session: Arc::clone(&request.session),
            vault_store: Arc::clone(&request.vault_store),
            spend_authorization: request.spend_authorization,
            public_account_uuid: request.public_account_uuid.clone(),
            token: request.token,
            amount: request.amount,
            recipient: request.recipient,
            verify_proof: request.verify_proof,
            gas_fee: request.gas_fee,
            incentive: request.incentive,
            authorization_limit: request.authorization_limit,
            progress_tx: request.progress_tx.clone(),
        },
        http,
    )
    .await?;
    let outcome = submit_prepared_sponsored_self_broadcast(
        DesktopPreparedSponsoredSelfBroadcastRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain,
            view_session: request.view_session,
            session: request.session,
            vault_store: request.vault_store,
            vault_password: request.vault_password,
            protected_software_seed_session: request.protected_software_seed_session,
            trezor_pin_matrix_provider: request.trezor_pin_matrix_provider,
            public_account_uuid: request.public_account_uuid,
            prepared: prepared.clone(),
            progress_tx: request.progress_tx,
            command_rx: request.command_rx,
            event_tx: request.event_tx,
        },
        http,
    )
    .await?;
    Ok(DesktopSponsoredSelfBroadcastResult { prepared, outcome })
}

pub async fn submit_desktop_sponsored_unshield_self_broadcast(
    request: DesktopSponsoredUnshieldSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<DesktopSponsoredSelfBroadcastResult> {
    let prepared = prepare_desktop_sponsored_unshield_calldata(
        DesktopSponsoredUnshieldCalldataRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.clone(),
            view_session: Arc::clone(&request.view_session),
            session: Arc::clone(&request.session),
            vault_store: Arc::clone(&request.vault_store),
            spend_authorization: request.spend_authorization,
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
    )
    .await?;
    let outcome = submit_prepared_sponsored_self_broadcast(
        DesktopPreparedSponsoredSelfBroadcastRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain,
            view_session: request.view_session,
            session: request.session,
            vault_store: request.vault_store,
            vault_password: request.vault_password,
            protected_software_seed_session: request.protected_software_seed_session,
            trezor_pin_matrix_provider: request.trezor_pin_matrix_provider,
            public_account_uuid: request.public_account_uuid,
            prepared: prepared.clone(),
            progress_tx: request.progress_tx,
            command_rx: request.command_rx,
            event_tx: request.event_tx,
        },
        http,
    )
    .await?;
    Ok(DesktopSponsoredSelfBroadcastResult { prepared, outcome })
}

pub async fn estimate_desktop_unshield_public_broadcaster_cost(
    request: DesktopUnshieldPublicBroadcasterEstimateRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterCostEstimate> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain.as_ref())?;
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }

    let policy = request.fee_policy;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(
        request.anchor_cache.as_ref(),
        request.chain_id,
        request.fee_token,
    );
    let candidates = public_broadcaster_candidates(
        &request.fee_rows,
        request.chain_id,
        request.fee_token,
        if request.unwrap || request.native_top_up.is_some() {
            Some(chain.relay_adapt_contract)
        } else {
            None
        },
        SystemTime::now(),
        policy,
        anchor_rate,
    );
    let broadcaster = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &request.selection,
        policy,
        &request.trust_filter,
    )?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls.clone(), http);
    let min_gas_price = buffered_gas_price_from_rpc_pool(&query_rpc_pool, &chain.gas).await?;
    let utxos = request.session.unspent_utxos();
    let same_token_fee = request.fee_token == request.token;
    let native_top_up = request
        .native_top_up
        .as_ref()
        .map(|_| {
            desktop_native_top_up_plan_for_estimate(
                request.chain_id,
                &chain,
                request.token,
                request.recipient,
                request.unwrap,
                request.amount,
            )
        })
        .transpose()?;
    let initial_fee_amount =
        initial_public_broadcaster_fee_amount(&broadcaster, min_gas_price, same_token_fee, || {
            let seed_split = public_broadcaster_amount_split_for_tokens_and_protocol(
                request.amount,
                U256::ZERO,
                request.fee_mode,
                same_token_fee,
                RAILGUN_PROTOCOL_FEE_BPS,
            )?;
            if let Some(native_top_up) = &native_top_up {
                return native_top_up_approximate_shape(
                    &utxos,
                    request.token,
                    request.fee_token,
                    seed_split.receiver_amount,
                    U256::ZERO,
                    native_top_up,
                );
            }
            let selection = unshield_selection_info_with_separate_broadcaster_fee_seed(
                &utxos,
                request.token,
                request.fee_token,
                seed_split.receiver_amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    U256::ZERO,
                    seed_split.fee_mode,
                    same_token_fee,
                    RAILGUN_PROTOCOL_FEE_BPS,
                )
            })?;
            Ok(unshield_approximate_shape(
                &selection,
                selection.max_spendable,
                request.unwrap,
            ))
        })?;

    let mut estimate = approximate_public_broadcaster_cost(
        broadcaster,
        request.token,
        request.fee_token,
        request.amount,
        request.fee_mode,
        RAILGUN_PROTOCOL_FEE_BPS,
        min_gas_price,
        initial_fee_amount,
        |split| {
            if let Some(native_top_up) = &native_top_up {
                return native_top_up_approximate_shape(
                    &utxos,
                    request.token,
                    request.fee_token,
                    split.receiver_amount,
                    split.fee_amount,
                    native_top_up,
                );
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
            Ok(unshield_approximate_shape(
                &selection,
                selection.max_spendable,
                request.unwrap,
            ))
        },
    )?;
    let reported_amounts = public_broadcaster_reported_amounts(
        request.token,
        request.fee_token,
        PublicBroadcasterAmountSplit {
            entered_amount: estimate.entered_amount,
            receiver_amount: estimate.receiver_amount,
            total_private_spend: estimate.total_private_spend,
            fee_amount: estimate.fee_amount,
            fee_mode: estimate.fee_mode,
        },
        RAILGUN_PROTOCOL_FEE_BPS,
        native_top_up.as_ref(),
    );
    estimate.recipient_amount = reported_amounts.recipient_amount;
    estimate.total_private_spend = reported_amounts.total_private_spend;
    estimate.protocol_fee_amount = reported_amounts.protocol_fee_amount;
    estimate.native_top_up = native_top_up;
    Ok(estimate)
}

pub fn estimate_desktop_send_self_broadcast_cost(
    utxos: &[Utxo],
    token: Address,
    amount: U256,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<DesktopSelfBroadcastCostEstimate> {
    let selection = send_selection_info(utxos, token, amount, false)
        .wrap_err("select POI-verified send notes for self-broadcast estimate")?;
    let shape = send_approximate_shape(&selection, selection.max_spendable);
    Ok(desktop_self_broadcast_cost_estimate(
        shape,
        quote,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        Vec::new(),
    ))
}

pub fn estimate_desktop_unshield_self_broadcast_cost(
    utxos: &[Utxo],
    token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<DesktopSelfBroadcastCostEstimate> {
    let receiver_amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    let shape = if let Some(native_top_up) = native_top_up {
        native_top_up_approximate_shape(
            utxos,
            token,
            token,
            receiver_amount,
            U256::ZERO,
            native_top_up,
        )?
    } else {
        let selection = unshield_selection_info(utxos, token, receiver_amount, false)
            .wrap_err("select POI-verified unshield notes for self-broadcast estimate")?;
        unshield_approximate_shape(&selection, selection.max_spendable, unwrap)
    };
    let protocol_fees = if let Some(native_top_up) = native_top_up {
        if token == native_top_up.wrapped_native_token {
            let combined_amount = native_top_up_required_wrapped_native_amount(
                token,
                native_top_up.wrapped_native_token,
                receiver_amount,
                native_top_up.native_amount,
            );
            vec![DesktopSelfBroadcastProtocolFee {
                token,
                amount: combined_amount
                    .saturating_sub(native_top_up_net_after_protocol_fee(combined_amount)),
            }]
        } else {
            vec![
                DesktopSelfBroadcastProtocolFee {
                    token,
                    amount: unshield_protocol_fee_amount_for_fee_mode(entered_amount, fee_mode)?,
                },
                DesktopSelfBroadcastProtocolFee {
                    token: native_top_up.wrapped_native_token,
                    amount: native_top_up
                        .wrapped_native_amount
                        .saturating_sub(native_top_up.native_amount),
                },
            ]
        }
    } else {
        vec![DesktopSelfBroadcastProtocolFee {
            token,
            amount: unshield_protocol_fee_amount_for_fee_mode(entered_amount, fee_mode)?,
        }]
    };
    Ok(desktop_self_broadcast_cost_estimate(
        shape,
        quote,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        protocol_fees,
    ))
}

fn desktop_self_broadcast_cost_estimate(
    shape: ApproximateTransactionShape,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    protocol_fees: Vec<DesktopSelfBroadcastProtocolFee>,
) -> DesktopSelfBroadcastCostEstimate {
    let gas_limit = approximate_public_broadcaster_gas(shape);
    DesktopSelfBroadcastCostEstimate {
        gas_limit,
        gas_cost: eip1559_gas_cost_projection(
            gas_limit,
            quote,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        ),
        protocol_fees,
    }
}

pub async fn estimate_desktop_send_public_broadcaster_cost(
    request: DesktopSendPublicBroadcasterEstimateRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterCostEstimate> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    parse_railgun_recipient(&request.recipient)?;

    let chain = effective_desktop_chain_config(request.chain_id, request.effective_chain.as_ref())?;
    let policy = request.fee_policy;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(
        request.anchor_cache.as_ref(),
        request.chain_id,
        request.fee_token,
    );
    let candidates = public_broadcaster_candidates(
        &request.fee_rows,
        request.chain_id,
        request.fee_token,
        None,
        SystemTime::now(),
        policy,
        anchor_rate,
    );
    let broadcaster = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &request.selection,
        policy,
        &request.trust_filter,
    )?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls.clone(), http);
    let min_gas_price = buffered_gas_price_from_rpc_pool(&query_rpc_pool, &chain.gas).await?;
    let utxos = request.session.unspent_utxos();
    let same_token_fee = request.fee_token == request.token;
    let initial_fee_amount =
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

    approximate_public_broadcaster_cost(
        broadcaster,
        request.token,
        request.fee_token,
        request.amount,
        request.fee_mode,
        U256::ZERO,
        min_gas_price,
        initial_fee_amount,
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
    )
}

pub async fn submit_desktop_unshield_public_broadcaster(
    request: DesktopUnshieldPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterSubmissionResult> {
    let waku = Arc::clone(&request.waku);
    let timeout = request.response_timeout;
    let republish_interval = request.republish_interval;
    let progress_tx = request.progress_tx.clone();
    let session = Arc::clone(&request.session);
    let prepared = prepare_desktop_unshield_public_broadcaster(request, http).await?;
    let pending_spent_inputs = prepared.plan.input_utxos();
    let result = submit_public_broadcaster_plan(
        waku,
        prepared.plan.call_to(),
        prepared.plan.call_data(),
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
        progress_tx,
        timeout,
        republish_interval,
    )
    .await?;
    mark_submitted_inputs_pending_spent(&session, &pending_spent_inputs, &result).await;
    Ok(result)
}

pub async fn submit_desktop_send_public_broadcaster(
    request: DesktopSendPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterSubmissionResult> {
    let waku = Arc::clone(&request.waku);
    let timeout = request.response_timeout;
    let republish_interval = request.republish_interval;
    let progress_tx = request.progress_tx.clone();
    let session = Arc::clone(&request.session);
    let prepared = prepare_desktop_send_public_broadcaster(request, http).await?;
    let pending_spent_inputs = prepared
        .plan
        .inputs
        .iter()
        .map(|input| input.utxo.clone())
        .collect::<Vec<_>>();
    let result = submit_public_broadcaster_plan(
        waku,
        prepared.plan.call.to,
        prepared.plan.call.data,
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
        progress_tx,
        timeout,
        republish_interval,
    )
    .await?;
    mark_submitted_inputs_pending_spent(&session, &pending_spent_inputs, &result).await;
    Ok(result)
}

pub async fn submit_desktop_unshield_self_broadcast(
    request: DesktopUnshieldSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<DesktopSelfBroadcastResult> {
    let prepared = prepare_desktop_unshield_plan_without_broadcaster_fee(
        DesktopUnshieldPlanRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.as_ref(),
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: request.spend_authorization,
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
    let mut result = submit_self_broadcast_plan(
        request.chain_id,
        request.effective_chain.as_ref(),
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        request
            .vault_password
            .as_ref()
            .map(|password| password.as_str()),
        request.protected_software_seed_session.as_deref(),
        request.trezor_pin_matrix_provider,
        request.public_account_uuid,
        Arc::clone(&request.session),
        prepared.plan.call_to(),
        prepared.plan.call_data(),
        pending_spent_inputs,
        prepared.native_top_up.is_some(),
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
    )
    .await?;
    result.native_top_up = prepared.native_top_up;
    Ok(result)
}

pub async fn submit_blocked_shield_rescue_self_broadcast(
    request: BlockedShieldRescueSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<DesktopSelfBroadcastResult> {
    let prepared = prepare_blocked_shield_rescue_plan(&request, http).await?;
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
    submit_self_broadcast_plan(
        request.chain_id,
        request.effective_chain.as_ref(),
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        Some(request.vault_password.as_str()),
        request.protected_software_seed_session.as_deref(),
        request.trezor_pin_matrix_provider,
        prepared.public_account_uuid,
        Arc::clone(&request.session),
        prepared.plan.call.to,
        prepared.plan.call.data,
        pending_spent_inputs,
        false,
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
    )
    .await
}

pub async fn submit_desktop_send_self_broadcast(
    request: DesktopSendSelfBroadcastRequest,
    http: &HttpContext,
) -> Result<DesktopSelfBroadcastResult> {
    let recipient = request.recipient.trim().to_string();
    let prepared = prepare_desktop_send_plan_without_broadcaster_fee(
        DesktopSendPlanRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.as_ref(),
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: request.spend_authorization,
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
    submit_self_broadcast_plan(
        request.chain_id,
        request.effective_chain.as_ref(),
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        request
            .vault_password
            .as_ref()
            .map(|password| password.as_str()),
        request.protected_software_seed_session.as_deref(),
        request.trezor_pin_matrix_provider,
        request.public_account_uuid,
        Arc::clone(&request.session),
        prepared.plan.call.to,
        prepared.plan.call.data,
        pending_spent_inputs,
        false,
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use railgun_wallet::tx::CompositePlanShape;

    fn test_sponsored_authorization(
        action: SponsoredActionKind,
        wrapped_native: Address,
        payer: Address,
    ) -> SponsoredAuthorization {
        sponsored_authorization(
            action,
            wrapped_native,
            payer,
            Address::from([0x30; 20]),
            sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment"),
            2,
            1,
            Address::from([0x31; 20]),
        )
    }

    fn test_zero_payment_authorization(
        action: SponsoredActionKind,
        wrapped_native: Address,
        payer: Address,
    ) -> SponsoredAuthorization {
        sponsored_authorization(
            action,
            wrapped_native,
            payer,
            Address::from([0x30; 20]),
            sponsorship_payment(100, 2, U256::from(200_u16), SponsoredIncentive::Standard)
                .expect("zero-delta payment"),
            2,
            1,
            Address::from([0x31; 20]),
        )
    }

    #[test]
    fn sponsored_action_fingerprint_binds_send_and_unshield_fields() {
        let token = Address::from([0x20; 20]);
        let send_recipient = AddressData {
            master_public_key: U256::from(1_u8),
            viewing_public_key: [2; 32],
        };
        let send = sponsored_send_action_fingerprint(1, token, U256::from(3_u8), &send_recipient);
        assert_ne!(
            send,
            sponsored_send_action_fingerprint(2, token, U256::from(3_u8), &send_recipient)
        );
        assert_ne!(
            send,
            sponsored_send_action_fingerprint(1, token, U256::from(4_u8), &send_recipient)
        );
        assert_ne!(
            send,
            sponsored_send_action_fingerprint(
                1,
                token,
                U256::from(3_u8),
                &AddressData {
                    master_public_key: U256::from(5_u8),
                    viewing_public_key: [2; 32],
                },
            )
        );

        let recipient = Address::from([0x21; 20]);
        let unshield = sponsored_unshield_action_fingerprint(
            1,
            token,
            U256::from(6_u8),
            recipient,
            false,
            None,
        );
        assert_ne!(
            unshield,
            sponsored_unshield_action_fingerprint(
                1,
                token,
                U256::from(6_u8),
                recipient,
                true,
                None,
            )
        );
        assert_ne!(
            unshield,
            sponsored_unshield_action_fingerprint(
                1,
                token,
                U256::from(6_u8),
                recipient,
                false,
                Some(&DesktopNativeTopUpPlan {
                    recipient,
                    wrapped_native_token: token,
                    native_amount: U256::ONE,
                    wrapped_native_amount: U256::from(2_u8),
                }),
            )
        );
    }

    #[test]
    fn sponsored_send_intent_reserves_one_exact_payer_leg() {
        let wrapped_native = Address::from([0x21; 20]);
        let payer = Address::from([0x22; 20]);
        let authorization =
            test_sponsored_authorization(SponsoredActionKind::Send, wrapped_native, payer);
        let request = SponsoredPrivateIntent::Send {
            token: Address::from([0x23; 20]),
            amount: U256::from(7_u8),
            recipient: AddressData {
                master_public_key: U256::from(8_u8),
                viewing_public_key: [9; 32],
            },
        }
        .mixed_request(&authorization, None, false, None)
        .expect("sponsored send request");

        assert_eq!(request.private_sends.len(), 1);
        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0],
            CompositeUnshieldLeg {
                token_address: wrapped_native,
                amount: authorization.gross_wrapped_native_spend,
                recipient: CompositeUnshieldRecipient::RelayAdapt,
                role: CompositeUnshieldLegRole::SponsoredWrappedNative,
            }
        );
        assert_eq!(
            request.relay_actions.expect("relay actions").calls,
            vec![
                CompositeRelayAction::UnwrapBase {
                    amount: authorization.builder_payment,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient: payer,
                    amount: authorization.builder_payment,
                },
            ]
        );
    }

    #[test]
    fn zero_delta_send_omits_sponsorship_output_and_payer_calls() {
        let wrapped_native = Address::from([0x21; 20]);
        let authorization = test_zero_payment_authorization(
            SponsoredActionKind::Send,
            wrapped_native,
            Address::from([0x22; 20]),
        );
        let request = SponsoredPrivateIntent::Send {
            token: Address::from([0x23; 20]),
            amount: U256::from(7_u8),
            recipient: AddressData {
                master_public_key: U256::from(8_u8),
                viewing_public_key: [9; 32],
            },
        }
        .mixed_request(&authorization, None, false, None)
        .expect("zero-delta send request");

        assert_eq!(request.private_sends.len(), 1);
        assert!(request.public_unshields.is_empty());
        assert!(request.relay_actions.is_none());
    }

    #[test]
    fn sponsored_wrapped_native_send_total_includes_private_recipient_amount() {
        let wrapped_native = Address::from([0x21; 20]);
        let recipient = AddressData {
            master_public_key: U256::from(8_u8),
            viewing_public_key: [9; 32],
        };
        let public_total = U256::from(11_u8);

        let total = sponsored_total_wrapped_native_spend(
            SponsoredPrivateIntent::Send {
                token: wrapped_native,
                amount: U256::from(7_u8),
                recipient,
            },
            public_total,
            wrapped_native,
        )
        .expect("wrapped-native total");

        assert_eq!(total, U256::from(18_u8));
    }

    #[test]
    fn sponsored_wrapped_native_send_total_rejects_overflow() {
        let wrapped_native = Address::from([0x21; 20]);
        let error = sponsored_total_wrapped_native_spend(
            SponsoredPrivateIntent::Send {
                token: wrapped_native,
                amount: U256::ONE,
                recipient: AddressData {
                    master_public_key: U256::from(8_u8),
                    viewing_public_key: [9; 32],
                },
            },
            U256::MAX,
            wrapped_native,
        )
        .expect_err("wrapped-native total must not saturate");

        assert_eq!(error, SponsorshipError::ArithmeticOverflow);
    }

    #[test]
    fn sponsored_wrapped_native_unshield_coalesces_primary_and_payer_output() {
        let wrapped_native = Address::from([0x24; 20]);
        let payer = Address::from([0x25; 20]);
        let recipient = Address::from([0x26; 20]);
        let primary_amount = U256::from(10_000_u64);
        let authorization =
            test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
        let request = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount: primary_amount,
            recipient,
            unwrap: true,
        }
        .mixed_request(&authorization, None, false, None)
        .expect("sponsored unshield request");

        assert!(request.private_sends.is_empty());
        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0].role,
            CompositeUnshieldLegRole::SponsoredWrappedNative
        );
        let primary_native_amount = native_top_up_net_after_protocol_fee(primary_amount);
        let expected_gross =
            gross_up_sponsorship_payment(authorization.builder_payment + primary_native_amount)
                .expect("combined gross");
        assert_eq!(request.public_unshields[0].amount, expected_gross);
        let calls = request.relay_actions.expect("relay actions").calls;
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[2],
            CompositeRelayAction::UnwrapBase {
                amount: primary_native_amount,
            }
        );
    }

    #[test]
    fn zero_delta_direct_unshield_omits_payer_calls() {
        let wrapped_native = Address::from([0x24; 20]);
        let token = Address::from([0x25; 20]);
        let recipient = Address::from([0x26; 20]);
        let authorization = test_zero_payment_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            Address::from([0x27; 20]),
        );
        let request = SponsoredPrivateIntent::Unshield {
            token,
            amount: U256::from(10_000_u64),
            recipient,
            unwrap: false,
        }
        .mixed_request(&authorization, None, false, None)
        .expect("zero-delta direct unshield request");

        assert!(request.private_sends.is_empty());
        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0].recipient,
            CompositeUnshieldRecipient::Public(recipient)
        );
        assert!(request.relay_actions.is_none());
    }

    #[test]
    fn zero_delta_unwrap_retains_recipient_calls_without_payer_calls() {
        let wrapped_native = Address::from([0x24; 20]);
        let recipient = Address::from([0x26; 20]);
        let authorization = test_zero_payment_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            Address::from([0x27; 20]),
        );
        let amount = U256::from(10_000_u64);
        let request = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount,
            recipient,
            unwrap: true,
        }
        .mixed_request(&authorization, None, false, None)
        .expect("zero-delta unwrap request");

        let recipient_amount = native_top_up_net_after_protocol_fee(amount);
        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request
                .relay_actions
                .expect("recipient relay actions")
                .calls,
            vec![
                CompositeRelayAction::UnwrapBase {
                    amount: recipient_amount,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient,
                    amount: recipient_amount,
                },
            ]
        );
    }

    #[test]
    fn zero_delta_wrapped_native_unshield_stays_direct() {
        let wrapped_native = Address::from([0x24; 20]);
        let recipient = Address::from([0x26; 20]);
        let authorization = test_zero_payment_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            Address::from([0x27; 20]),
        );
        let request = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount: U256::from(10_000_u64),
            recipient,
            unwrap: false,
        }
        .mixed_request(&authorization, None, false, None)
        .expect("zero-delta wrapped-native unshield request");

        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0].role,
            CompositeUnshieldLegRole::Primary
        );
        assert_eq!(
            request.public_unshields[0].recipient,
            CompositeUnshieldRecipient::Public(recipient)
        );
        assert!(request.relay_actions.is_none());
    }

    #[test]
    fn sponsored_wrapped_native_output_coalesces_recipient_and_payer_output() {
        let wrapped_native = Address::from([0x27; 20]);
        let payer = Address::from([0x28; 20]);
        let recipient = Address::from([0x29; 20]);
        let primary_amount = U256::from(10_000_u64);
        let authorization =
            test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
        let request = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount: primary_amount,
            recipient,
            unwrap: false,
        }
        .mixed_request(&authorization, None, false, None)
        .expect("sponsored wrapped-native output request");

        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0].role,
            CompositeUnshieldLegRole::SponsoredWrappedNative
        );
        let recipient_amount = native_top_up_net_after_protocol_fee(primary_amount);
        assert_eq!(
            request.public_unshields[0].amount,
            gross_up_sponsorship_payment(authorization.builder_payment + recipient_amount)
                .expect("combined gross")
        );
        let calls = request.relay_actions.expect("relay actions").calls;
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[2],
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::Erc20(wrapped_native),
                recipient,
                amount: recipient_amount,
            }
        );
    }

    #[test]
    fn sponsored_dai_unshield_coalesces_native_top_up_with_builder_payment() {
        let wrapped_native = Address::from([0x41; 20]);
        let token = Address::from([0x42; 20]);
        let payer = Address::from([0x43; 20]);
        let recipient = Address::from([0x44; 20]);
        let authorization =
            test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
        let top_up = DesktopNativeTopUpPlan {
            recipient,
            wrapped_native_token: wrapped_native,
            native_amount: U256::from(100_u64),
            wrapped_native_amount: native_top_up_wrapped_native_amount(U256::from(100_u64)),
        };
        let request = SponsoredPrivateIntent::Unshield {
            token,
            amount: U256::from(1_000_u64),
            recipient,
            unwrap: false,
        }
        .mixed_request(&authorization, Some(&top_up), false, None)
        .expect("sponsored top-up request");

        assert_eq!(request.public_unshields.len(), 2);
        assert_eq!(
            request.public_unshields[0].role,
            CompositeUnshieldLegRole::SponsoredWrappedNative
        );
        let expected_gross =
            gross_up_sponsorship_payment(authorization.builder_payment + top_up.native_amount)
                .expect("combined gross");
        assert_eq!(request.public_unshields[0].amount, expected_gross);
        assert_eq!(
            request.public_unshields[1].role,
            CompositeUnshieldLegRole::Primary
        );
        let calls = request.relay_actions.expect("relay actions").calls;
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[1],
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient: payer,
                amount: authorization.builder_payment,
            }
        );
        assert_eq!(
            calls[3],
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient,
                amount: top_up.native_amount,
            }
        );
    }

    #[test]
    fn sponsored_wrapped_native_top_up_coalesces_all_recipient_and_builder_routing() {
        let wrapped_native = Address::from([0x51; 20]);
        let payer = Address::from([0x52; 20]);
        let recipient = Address::from([0x53; 20]);
        let primary_amount = U256::from(1_000_u64);
        let authorization =
            test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
        let top_up = DesktopNativeTopUpPlan {
            recipient,
            wrapped_native_token: wrapped_native,
            native_amount: U256::from(100_u64),
            wrapped_native_amount: native_top_up_wrapped_native_amount(U256::from(100_u64)),
        };
        let request = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount: primary_amount,
            recipient,
            unwrap: false,
        }
        .mixed_request(&authorization, Some(&top_up), false, None)
        .expect("sponsored wrapped-native top-up request");

        assert_eq!(request.public_unshields.len(), 1);
        assert_eq!(
            request.public_unshields[0].role,
            CompositeUnshieldLegRole::SponsoredWrappedNative
        );
        let calls = request.relay_actions.expect("relay actions").calls;
        let combined_recipient_amount = native_top_up_required_wrapped_native_amount(
            wrapped_native,
            wrapped_native,
            primary_amount,
            top_up.native_amount,
        );
        let recipient_amount =
            native_top_up_net_after_protocol_fee(combined_recipient_amount) - top_up.native_amount;
        assert_eq!(
            calls,
            vec![
                CompositeRelayAction::UnwrapBase {
                    amount: authorization.builder_payment,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient: payer,
                    amount: authorization.builder_payment,
                },
                CompositeRelayAction::UnwrapBase {
                    amount: top_up.native_amount,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient,
                    amount: top_up.native_amount,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::Erc20(wrapped_native),
                    recipient,
                    amount: recipient_amount,
                },
            ]
        );
        let expected_net = authorization.builder_payment + top_up.native_amount + recipient_amount;
        assert_eq!(
            request.public_unshields[0].amount,
            gross_up_sponsorship_payment(expected_net).expect("combined gross")
        );
    }

    fn sponsored_public_output(
        unshield_index: usize,
        transaction_index: usize,
        token: Address,
        amount: U256,
    ) -> MixedPublicPlannedOutput {
        MixedPublicPlannedOutput {
            unshield_index,
            transaction_index,
            output_index: 0,
            token_address: token,
            amount,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::SponsoredWrappedNative,
            note: Note::new_unshield(Address::ZERO, token, amount),
        }
    }

    #[test]
    fn sponsored_output_validation_aggregates_chunks_for_one_logical_leg() {
        let wrapped_native = Address::from([0x61; 20]);
        let outputs = vec![
            sponsored_public_output(0, 0, wrapped_native, U256::from(4_u8)),
            sponsored_public_output(0, 1, wrapped_native, U256::from(6_u8)),
        ];

        assert_eq!(
            sponsored_wrapped_native_output_total(&outputs, 0, wrapped_native),
            Ok(U256::from(10_u8))
        );
        assert_eq!(
            sponsored_wrapped_native_output_total(&outputs, 1, wrapped_native),
            Err(SponsorshipError::SponsoredPlanShapeChangeRequired)
        );

        let overflowing = vec![
            sponsored_public_output(0, 0, wrapped_native, U256::MAX),
            sponsored_public_output(0, 1, wrapped_native, U256::ONE),
        ];
        assert_eq!(
            sponsored_wrapped_native_output_total(&overflowing, 0, wrapped_native),
            Err(SponsorshipError::ArithmeticOverflow)
        );
    }

    #[test]
    fn pinned_input_and_shape_failures_have_named_outcome() {
        let error = sponsored_rebuild_error(BuildError::PinnedInputsInsufficient(U256::ONE));
        assert_eq!(
            error.downcast_ref::<SponsorshipError>(),
            Some(&SponsorshipError::SponsoredPlanShapeChangeRequired)
        );
        let error = sponsored_rebuild_error(BuildError::CompositePlanShapeChanged {
            expected: CompositePlanShape {
                transaction_count: 1,
                input_count: 1,
                private_output_count: 1,
                public_output_count: 1,
                relay_call_count: 2,
                uses_relay_adapt: true,
            },
            actual: CompositePlanShape {
                transaction_count: 2,
                input_count: 1,
                private_output_count: 1,
                public_output_count: 1,
                relay_call_count: 2,
                uses_relay_adapt: true,
            },
        });
        assert_eq!(
            error.downcast_ref::<SponsorshipError>(),
            Some(&SponsorshipError::SponsoredPlanShapeChangeRequired)
        );
    }

    fn test_utxo(token: Address, value: U256) -> Utxo {
        test_utxo_at(token, value, 0)
    }

    fn test_utxo_at(token: Address, value: U256, position: u64) -> Utxo {
        Utxo::new(
            Note::new_unshield(Address::ZERO, token, value),
            0,
            position,
            UtxoSource {
                tx_hash: FixedBytes::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Transact,
        )
    }

    fn test_chain_config(wrapped_native: Address) -> EffectiveDesktopChainConfig {
        EffectiveDesktopChainConfig {
            rpc_urls: Vec::new(),
            railgun_contract: Address::ZERO,
            relay_adapt_contract: Address::ZERO,
            wrapped_native_token: Some(wrapped_native),
            finality_depth: 1,
            gas: settings::EffectiveChainGasSettings {
                gas_limit_buffer: GAS_LIMIT_BUFFER,
                gas_price_buffer_numerator: GAS_PRICE_BUFFER_NUMERATOR as u64,
                gas_price_buffer_denominator: GAS_PRICE_BUFFER_DENOMINATOR as u64,
            },
        }
    }

    #[test]
    fn sponsored_quote_uses_buffered_shape_without_extra_percentage_headroom() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let effective_chain =
            settings::build_effective_chain_configs(&settings::WalletSettings::default())
                .expect("effective chains")
                .remove(&1)
                .expect("ethereum config");
        let chain = test_chain_config(wrapped_native);
        let payer = effective_chain.coinbase_payer.expect("coinbase payer");
        let signer = Address::from([0x33; 20]);
        let amount = U256::from(20_000_000_000_000_000_u128);
        let recipient = Address::from([0x34; 20]);
        let utxos = vec![test_utxo_at(
            wrapped_native,
            U256::from(10_000_000_000_000_000_000_u128),
            0,
        )];
        let intent = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount,
            recipient,
            unwrap: false,
        };
        let expected = sponsored_provisional_payment_for_intent(
            1,
            &chain,
            &utxos,
            wrapped_native,
            payer,
            signer,
            intent,
            None,
            150_000_000,
            10_000_000,
            U256::ZERO,
            SponsoredIncentive::Economy,
        )
        .expect("provisional payment");

        let quote = quote_sponsored_unshield_authorization_limit(
            1,
            &effective_chain,
            &utxos,
            wrapped_native,
            amount,
            FeeHandlingMode::DeductFromAmount,
            recipient,
            false,
            None,
            150_000_000,
            10_000_000,
            U256::ZERO,
            SponsoredIncentive::Economy,
            signer,
        )
        .expect("sponsored quote");

        assert_eq!(quote.max_transaction_gas_limit, expected.outer_gas_limit);
    }

    #[test]
    fn sponsored_quote_credits_snapshot_balance_against_builder_funding() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let effective_chain =
            settings::build_effective_chain_configs(&settings::WalletSettings::default())
                .expect("effective chains")
                .remove(&1)
                .expect("ethereum config");
        let amount = U256::from(20_000_000_000_000_000_u128);
        let recipient = Address::from([0x34; 20]);
        let signer = Address::from([0x33; 20]);
        let utxos = vec![test_utxo_at(
            wrapped_native,
            U256::from(10_000_000_000_000_000_000_u128),
            0,
        )];
        let signer_balance = U256::from(100_000_000_000_000_u128);

        let zero_balance = quote_sponsored_unshield_authorization_limit(
            1,
            &effective_chain,
            &utxos,
            wrapped_native,
            amount,
            FeeHandlingMode::DeductFromAmount,
            recipient,
            false,
            None,
            150_000_000,
            10_000_000,
            U256::ZERO,
            SponsoredIncentive::Economy,
            signer,
        )
        .expect("zero-balance quote");
        let credited = quote_sponsored_unshield_authorization_limit(
            1,
            &effective_chain,
            &utxos,
            wrapped_native,
            amount,
            FeeHandlingMode::DeductFromAmount,
            recipient,
            false,
            None,
            150_000_000,
            10_000_000,
            signer_balance,
            SponsoredIncentive::Economy,
            signer,
        )
        .expect("balance-aware quote");

        assert_eq!(credited.signer_native_balance_snapshot, signer_balance);
        assert!(
            credited
                .maximum_payment()
                .expect("credited payment")
                .gross_wrapped_native_spend
                < zero_balance
                    .maximum_payment()
                    .expect("zero-balance payment")
                    .gross_wrapped_native_spend
        );
    }

    #[test]
    fn sponsored_quote_does_not_report_an_intermediate_required_balance() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let effective_chain =
            settings::build_effective_chain_configs(&settings::WalletSettings::default())
                .expect("effective chains")
                .remove(&1)
                .expect("ethereum config");
        let token = Address::from([0x81; 20]);
        let signer = Address::from([0x82; 20]);
        let recipient = Address::from([0x83; 20]);
        for available in [U256::ZERO, U256::from(1_000_000_u64)] {
            let mut utxos = vec![test_utxo_at(
                token,
                U256::from(10_000_000_000_000_000_000_u128),
                0,
            )];
            if !available.is_zero() {
                utxos.push(test_utxo_at(wrapped_native, available, 1));
            }

            let error = quote_sponsored_unshield_authorization_limit(
                1,
                &effective_chain,
                &utxos,
                token,
                U256::from(1_000_000_000_000_000_000_u128),
                FeeHandlingMode::DeductFromAmount,
                recipient,
                false,
                None,
                1_000_000_000_000,
                100_000_000,
                U256::ZERO,
                SponsoredIncentive::Standard,
                signer,
            )
            .expect_err("high gas price exceeds wrapped-native balance");
            assert!(matches!(
                error.downcast_ref::<SponsorshipError>(),
                Some(SponsorshipError::InsufficientWrappedNativeForQuote {
                    available: actual,
                }) if *actual == available
            ));
        }
    }

    #[test]
    fn sponsored_wrapped_native_quote_binds_canonical_coalesced_total() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let effective_chain =
            settings::build_effective_chain_configs(&settings::WalletSettings::default())
                .expect("effective chains")
                .remove(&1)
                .expect("ethereum config");
        let payer = effective_chain.coinbase_payer.expect("coinbase payer");
        let relay_adapt: Address = effective_chain
            .relay_adapt_contract
            .parse()
            .expect("relay adapt");
        let signer = Address::from([0x71; 20]);
        let recipient = Address::from([0x72; 20]);
        let entered_amount = U256::from(1_596_000_000_000_211_u128);
        let utxos = vec![test_utxo(
            wrapped_native,
            U256::from(10_000_000_000_000_000_000_u128),
        )];

        for fee_mode in [
            FeeHandlingMode::DeductFromAmount,
            FeeHandlingMode::AddToAmount,
        ] {
            let quote = quote_sponsored_unshield_authorization_limit(
                1,
                &effective_chain,
                &utxos,
                wrapped_native,
                entered_amount,
                fee_mode,
                recipient,
                false,
                None,
                150_000_000,
                10_000_000,
                U256::ZERO,
                SponsoredIncentive::Economy,
                signer,
            )
            .expect("sponsored quote");
            let amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)
                .expect("receiver amount");
            let intent = SponsoredPrivateIntent::Unshield {
                token: wrapped_native,
                amount,
                recipient,
                unwrap: false,
            };
            let payment = quote.maximum_payment().expect("maximum payment");
            let authorization = sponsored_authorization(
                SponsoredActionKind::Unshield,
                wrapped_native,
                payer,
                relay_adapt,
                payment,
                quote.max_fee_per_gas,
                quote.max_priority_fee_per_gas,
                signer,
            );
            let request = intent
                .mixed_request(&authorization, None, false, None)
                .expect("canonical request");
            let (_, canonical_total) = optional_sponsored_wrapped_native_leg_amount(&request)
                .expect("valid sponsored leg")
                .expect("sponsored leg");
            let fingerprint = sponsored_unshield_action_fingerprint(
                1,
                wrapped_native,
                amount,
                recipient,
                false,
                None,
            );

            assert_eq!(quote.max_total_wrapped_native_spend, canonical_total);
            assert_eq!(
                validate_sponsored_authorization_limit(
                    quote,
                    fingerprint,
                    authorization,
                    canonical_total,
                ),
                Ok(())
            );
            assert_eq!(
                validate_sponsored_authorization_limit(
                    SponsoredAuthorizationLimit {
                        max_total_wrapped_native_spend: canonical_total - U256::ONE,
                        ..quote
                    },
                    fingerprint,
                    authorization,
                    canonical_total,
                ),
                Err(SponsorshipError::AuthorizationLimitExceeded)
            );
        }
    }

    #[test]
    fn native_top_up_estimate_rejects_unwrap_as_unsupported() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let chain = test_chain_config(wrapped_native);
        let error = desktop_native_top_up_plan_for_estimate(
            1,
            &chain,
            wrapped_native,
            Address::from([0x52; 20]),
            true,
            U256::ONE,
        )
        .expect_err("unwrap-to-native cannot be combined with native top-up");
        assert_eq!(
            error.to_string(),
            "native top-up cannot be combined with unwrap-to-native output"
        );
    }

    #[test]
    fn native_top_up_plan_validation_counts_wrapped_native_broadcaster_fee() {
        let wrapped_native = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
        let token = Address::from([0x51; 20]);
        let recipient = Address::from([0x52; 20]);
        let receiver_amount = U256::from(25_u64);
        let chain = test_chain_config(wrapped_native);
        let policy = native_top_up_policy_for_chain(1).expect("ethereum native top-up policy");
        let required_without_fee = native_top_up_required_wrapped_native_amount(
            token,
            wrapped_native,
            receiver_amount,
            policy.top_up_amount,
        );
        let utxos = vec![test_utxo(wrapped_native, required_without_fee)];

        desktop_native_top_up_plan_from_unshield_fields(
            1,
            &chain,
            token,
            recipient,
            false,
            receiver_amount,
            Some(wrapped_native),
            U256::ZERO,
            &utxos,
        )
        .expect("zero wrapped-native broadcaster fee fits available balance");

        let fee_amount = U256::from(1_u64);
        let error = desktop_native_top_up_plan_from_unshield_fields(
            1,
            &chain,
            token,
            recipient,
            false,
            receiver_amount,
            Some(wrapped_native),
            fee_amount,
            &utxos,
        )
        .expect_err("wrapped-native broadcaster fee should require additional balance");
        let expected_required = required_without_fee.saturating_add(fee_amount);
        let message = error.to_string();
        assert!(message.contains("native top-up wrapped-native max spendable"));
        assert!(message.contains(&format!("required: {expected_required}")));
    }
}
