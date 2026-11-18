use super::*;

pub(in crate::desktop) async fn submit_self_broadcast_plan(
    activity: Option<&ActivitySubmission>,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    view_session: &vault::DesktopViewSession,
    vault_store: &vault::DesktopVaultStore,
    vault_password: Option<&str>,
    protected_seed_session: Option<&vault::ProtectedSoftwareSeedSession>,
    spend_authorization: Option<DesktopPrivateSpendAuthorization>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    public_account_uuid: String,
    session: Arc<WalletSession>,
    transaction: TransactionRequest,
    pending_spent_inputs: Vec<Utxo>,
    native_top_up_cannot_pay_current_gas: bool,
    gas_fee: SelfBroadcastGasFeeSelection,
    progress_tx: Option<TransactionGenerationProgressSender>,
    mut command_rx: Option<SelfBroadcastCommandReceiver>,
    event_tx: Option<SelfBroadcastSessionEventSender>,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSelfBroadcastResult> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let gas_payer = self_broadcast_gas_payer(vault_store, view_session, &public_account_uuid)?;
    if let Some(activity) = activity {
        activity.set_gas_payer(gas_payer);
    }
    // The pools resolve here, before the outer transaction is signed, isolated over `client`.
    let sensitive_route = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)?
        .isolated(client.clone());
    let query_rpc_pool = query_rpc_pool_for_route(&sensitive_route, http);
    let submit_pool = query_rpc_pool_for_route(
        &chain
            .rpc_route
            .for_class(RpcTrafficClass::Submission)?
            .isolated(client.clone()),
        http,
    );
    let signer =
        if let Some(authorization @ DesktopPrivateSpendAuthorization::HardwareExecutor(hardware)) =
            spend_authorization.as_ref()
            && hardware.is_gas_payment_for(&public_account_uuid)
        {
            let account = vault_store
                .list_public_accounts_for_session(view_session, true)?
                .into_iter()
                .find(|account| {
                    account.public_account_uuid == public_account_uuid
                        && account.is_available_on_chain(chain_id)
                })
                .ok_or_else(|| eyre!("reviewed gas payer is unavailable"))?;
            let owner = session
                .executor_owner()
                .ok_or_else(|| eyre!("Open the owning wallet and chain before signing"))?;
            let (signer, guard) = Box::pin(owner.admit_authorized_gas_signer(
                view_session,
                &account,
                authorization,
                client,
            ))
            .await?;
            VaultedPublicSigner::Executor(signer, guard)
        } else {
            admitted_public_signer(
                vault_store,
                view_session,
                vault_password,
                &public_account_uuid,
                protected_seed_session,
                None,
                trezor_pin_matrix_provider,
                session.executor_owner().as_ref(),
                chain_id,
                client,
            )
            .await?
        };
    // Gas-signer admission is the last consumer of the private/executor approval.
    drop(spend_authorization);
    signer.while_active(async {
    if signer.address() != gas_payer {
        return Err(eyre!(
            "selected public account signer address does not match account metadata"
        ));
    }
    let executor = if let Some(owner) = session.executor_owner() {
        owner
            .transaction_identity(&transaction)?
            .map(|identity| (owner, identity))
    } else {
        None
    };
    let mut next_gas_fee = gas_fee;
    let mut submitted_attempts = Vec::new();
    let mut nonce = None;
    let mut observer = None;

    loop {
        update_transaction_generation_stage(
            progress_tx.as_ref(),
            TransactionGenerationStage::EstimatingSelfBroadcastGas,
        );
        let preflight = match self_broadcast_preflight_from_rpc_pool(
            &query_rpc_pool,
            http,
            &sensitive_route,
            chain_id,
            gas_payer,
            transaction.clone(),
            next_gas_fee,
            &chain.gas,
            nonce,
        )
        .await
        {
            Ok(preflight) => preflight,
            Err(error) => {
                let message = self_broadcast_preflight_error_message(
                    &error,
                    native_top_up_cannot_pay_current_gas,
                );
                emit_self_broadcast_event(
                    event_tx.as_ref(),
                    SelfBroadcastSessionEvent::StepFailed {
                        stage: TransactionGenerationStage::EstimatingSelfBroadcastGas,
                        message,
                    },
                );
                let Some(command) = recv_self_broadcast_command(&mut command_rx).await else {
                    return if native_top_up_cannot_pay_current_gas
                        && is_self_broadcast_insufficient_native_gas_error(&error)
                    {
                        Err(error.wrap_err(NATIVE_TOP_UP_SELF_BROADCAST_GAS_GUIDANCE))
                    } else {
                        Err(error)
                    };
                };
                next_gas_fee = command.gas_fee;
                continue;
            }
        };
        nonce = Some(preflight.nonce);

        if observer.is_none() {
            let created = BlockObserver::establish(
                Arc::clone(&query_rpc_pool),
                chain.finality_depth,
                http.rpc_broker(),
                &sensitive_route,
            )
            .await?;
            observer = Some(PublicTransactionObservationGuard::new(
                created,
                transaction_tracking,
            )?);
        }

        update_transaction_generation_stage(
            progress_tx.as_ref(),
            TransactionGenerationStage::SigningSelfBroadcast,
        );
        match submit_self_broadcast_attempt(
            preflight,
            &submit_pool,
            http.network_mode(),
            &signer,
            &session,
            &pending_spent_inputs,
            event_tx.as_ref(),
            transaction_tracking,
            activity,
            observer
                .as_mut()
                .expect("self-broadcast observer established"),
            &mut submitted_attempts,
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                if is_trezor_app_passphrase_required_error(&error) {
                    return Err(error.wrap_err(TREZOR_SELF_BROADCAST_RESTART_GUIDANCE));
                }
                let message = report_chain_string(&error);
                let (stage, message) = if submitted_attempts.is_empty() {
                    (TransactionGenerationStage::SigningSelfBroadcast, message)
                } else {
                    (
                        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
                        format!(
                            "Broadcast outcome is uncertain; continuing block observation: {message}"
                        ),
                    )
                };
                emit_self_broadcast_event(
                    event_tx.as_ref(),
                    SelfBroadcastSessionEvent::StepFailed { stage, message },
                );
                if submitted_attempts.is_empty() {
                    let Some(command) = recv_self_broadcast_command(&mut command_rx).await else {
                        return Err(error);
                    };
                    next_gas_fee = command.gas_fee;
                    continue;
                }
            }
        }
        update_transaction_generation_stage(
            progress_tx.as_ref(),
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        );

        loop {
            match wait_for_self_broadcast_poll_or_command(&mut command_rx).await {
                None => {
                    let observation = observer
                        .as_mut()
                        .expect("self-broadcast observer established")
                        .poll()
                        .await;
                    if let Some((winner_index, mut tx)) =
                        self_broadcast_observation_outcome(observation, &submitted_attempts)?
                    {
                        let winner = self_broadcast_winner_output(&submitted_attempts, winner_index);
                        if let Some(receipt) = tx.receipt() && executor.is_none() {
                            session
                                .mark_pending_spent_utxos(
                                    &pending_spent_inputs,
                                    parse_submitted_tx_hash(&receipt.tx_hash),
                                )
                                .await;
                        }
                        if let Some((owner, identity)) = &executor
                            && let SelfBroadcastTxOutcome::Receipt(receipt) = tx
                        {
                            let reconciliation = match &signer {
                                VaultedPublicSigner::Executor(_, guard) => guard.reconcile_account(owner, identity.operation(), client).await,
                                _ => owner.reconcile_account(identity.operation(), client).await,
                            };
                            let state = reconciliation
                                .ok()
                                .and_then(|report| report.record().payload_state(identity.payload()))
                                .unwrap_or(vault::ExecutorPayloadState::Pending);
                            tx = SelfBroadcastTxOutcome::ExecutorReceipt { receipt, state };
                        }
                        return Ok(DesktopSelfBroadcastResult {
                            chain_id,
                            public_account_uuid,
                            gas_payer,
                            gas_limit: winner.gas_limit,
                            rpc_gas_price: winner.rpc_gas_price,
                            max_fee_per_gas: winner.max_fee_per_gas,
                            max_priority_fee_per_gas: winner.max_priority_fee_per_gas,
                            estimated_native_gas_cost: winner.estimated_native_gas_cost,
                            live_native_balance: winner.live_native_balance,
                            tx,
                            attempts: submitted_attempts
                                .iter()
                                .map(|attempt| attempt.info.clone())
                                .collect(),
                            native_top_up: None,
                        });
                    }
                }
                Some(command) => {
                    let Some(nonce) = nonce else {
                        next_gas_fee = command.gas_fee;
                        break;
                    };
                    let gas_limit = submitted_attempts
                        .last()
                        .map_or(0, |attempt| attempt.info.gas_limit);
                    let replacement = match self_broadcast_replacement_preflight_from_rpc_pool(
                        &query_rpc_pool,
                        http,
                        &sensitive_route,
                        chain_id,
                        gas_payer,
                        transaction.clone(),
                        command.gas_fee,
                        gas_limit,
                        nonce,
                    )
                    .await
                    {
                        Ok(preflight) => preflight,
                        Err(error) => {
                            emit_self_broadcast_event(
                                event_tx.as_ref(),
                                SelfBroadcastSessionEvent::AttemptRejected {
                                    stage: TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
                                    message: self_broadcast_preflight_error_message(
                                        &error,
                                        native_top_up_cannot_pay_current_gas,
                                    ),
                                },
                            );
                            continue;
                        }
                    };
                    update_transaction_generation_stage(
                        progress_tx.as_ref(),
                        TransactionGenerationStage::SigningSelfBroadcast,
                    );
                    let previous_attempt_count = submitted_attempts.len();
                    match submit_self_broadcast_attempt(
                        replacement,
                        &submit_pool,
                        http.network_mode(),
                        &signer,
                        &session,
                        &pending_spent_inputs,
                        event_tx.as_ref(),
                        transaction_tracking,
                        activity,
                        observer.as_mut().expect("self-broadcast observer established"),
                        &mut submitted_attempts,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(error) => {
                            let message = self_broadcast_signing_error_message(&error);
                            let event = if submitted_attempts.len() > previous_attempt_count {
                                SelfBroadcastSessionEvent::StepFailed {
                                    stage: TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
                                    message: format!("Broadcast outcome is uncertain; continuing block observation: {message}"),
                                }
                            } else {
                                SelfBroadcastSessionEvent::AttemptRejected {
                                    stage: TransactionGenerationStage::SigningSelfBroadcast,
                                    message,
                                }
                            };
                            emit_self_broadcast_event(event_tx.as_ref(), event);
                        }
                    }
                    update_transaction_generation_stage(
                        progress_tx.as_ref(),
                        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
                    );
                }
            }
        }
    }
    }).await
}

pub(in crate::desktop) fn emit_self_broadcast_event(
    event_tx: Option<&SelfBroadcastSessionEventSender>,
    event: SelfBroadcastSessionEvent,
) {
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(event);
    }
}

pub(super) fn emit_refreshed_self_broadcast_hardware_session(
    event_tx: Option<&SelfBroadcastSessionEventSender>,
    signer: &VaultedPublicSigner,
) {
    match signer.refreshed_hardware_session() {
        Ok(Some(session)) => emit_self_broadcast_event(
            event_tx,
            SelfBroadcastSessionEvent::HardwareProfileSessionRefreshed { session },
        ),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                target: LOG_TARGET,
                %error,
                "failed to read refreshed self-broadcast signer session"
            );
        }
    }
}

pub(crate) fn report_chain_string(error: &Report) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

pub(super) fn self_broadcast_signing_error_message(error: &Report) -> String {
    let message = report_chain_string(error);
    if is_trezor_app_passphrase_required_error_message(&message) {
        format!("{message}: {TREZOR_SELF_BROADCAST_RESTART_GUIDANCE}")
    } else {
        message
    }
}

pub(crate) fn self_broadcast_preflight_error_message(
    error: &Report,
    native_top_up_cannot_pay_current_gas: bool,
) -> String {
    let message = report_chain_string(error);
    if native_top_up_cannot_pay_current_gas
        && is_self_broadcast_insufficient_native_gas_error(error)
    {
        format!("{message}: {NATIVE_TOP_UP_SELF_BROADCAST_GAS_GUIDANCE}")
    } else {
        message
    }
}

pub(super) fn is_trezor_app_passphrase_required_error(error: &Report) -> bool {
    is_trezor_app_passphrase_required_error_message(&report_chain_string(error))
}

fn is_trezor_app_passphrase_required_error_message(message: &str) -> bool {
    message.contains(TREZOR_APP_PASSPHRASE_ERROR_TEXT)
}

pub(in crate::desktop) async fn recv_self_broadcast_command(
    command_rx: &mut Option<SelfBroadcastCommandReceiver>,
) -> Option<SelfBroadcastCommand> {
    let command_rx = command_rx.as_mut()?;
    command_rx.recv().await
}

/// `None` requests the next receipt poll; a closed command stream is not a poll tick.
pub(super) async fn wait_for_self_broadcast_poll_or_command(
    command_rx: &mut Option<SelfBroadcastCommandReceiver>,
) -> Option<SelfBroadcastCommand> {
    let poll = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(poll);
    loop {
        tokio::select! {
            () = &mut poll => return None,
            command = recv_self_broadcast_command(command_rx), if command_rx.is_some() => {
                if command.is_some() {
                    return command;
                }
                *command_rx = None;
            }
        }
    }
}

pub(in crate::desktop) async fn submit_self_broadcast_attempt(
    preflight: SelfBroadcastPreflight,
    query_rpc_pool: &QueryRpcPool,
    network_mode: WalletNetworkMode,
    signer: &VaultedPublicSigner,
    session: &WalletSession,
    pending_spent_inputs: &[Utxo],
    event_tx: Option<&SelfBroadcastSessionEventSender>,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    activity: Option<&ActivitySubmission>,
    observer: &mut BlockObserver,
    submitted_attempts: &mut Vec<SubmittedSelfBroadcastAttempt>,
) -> Result<()> {
    sign_send_self_broadcast_transaction(
        query_rpc_pool,
        network_mode,
        signer,
        preflight.tx_req,
        session,
        pending_spent_inputs,
        event_tx,
        |tx_hash| {
            if let Some(context) = transaction_tracking {
                context.ensure_open()?;
            }
            // A failed RPC response cannot prove that the signed transaction was not sent.
            // Retain its metadata and index before handing it to the transport.
            let attempt_id = submitted_attempts.len();
            submitted_attempts.push(SubmittedSelfBroadcastAttempt {
                info: SelfBroadcastAttemptInfo {
                    tx_hash: hex::encode_prefixed(tx_hash),
                    nonce: preflight.nonce,
                    gas_limit: preflight.gas_limit,
                    max_fee_per_gas: preflight.max_fee_per_gas,
                    max_priority_fee_per_gas: preflight.max_priority_fee_per_gas,
                },
                rpc_gas_price: preflight.rpc_gas_price,
                estimated_native_gas_cost: preflight.estimated_native_gas_cost,
                live_native_balance: preflight.live_native_balance,
            });
            observer.register(tx_hash, attempt_id);
            if let Some(activity) = activity {
                activity.add_attempt(tx_hash);
            }
            Ok(())
        },
    )
    .await?;
    emit_self_broadcast_event(
        event_tx,
        SelfBroadcastSessionEvent::AttemptSubmitted(
            submitted_attempts
                .last()
                .expect("signed attempt registered")
                .info
                .clone(),
        ),
    );
    Ok(())
}

pub(in crate::desktop) fn self_broadcast_gas_payer(
    vault_store: &vault::DesktopVaultStore,
    view_session: &vault::DesktopViewSession,
    public_account_uuid: &str,
) -> Result<Address> {
    vault_store
        .list_active_public_accounts_for_session(view_session)
        .wrap_err("load active public accounts")?
        .into_iter()
        .find(|account| account.public_account_uuid == public_account_uuid)
        .map(|account| account.address)
        .ok_or_else(|| eyre!("selected gas payer is not an active Public account"))
}
