#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::public_wallet) fn public_action_pools(
    route: &crate::RpcChainRoute,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicActionPools> {
    let sensitive = route
        .for_class(crate::RpcTrafficClass::Sensitive)?
        .isolated(client.clone());
    let submit = route
        .for_class(crate::RpcTrafficClass::Submission)?
        .isolated(client.clone());
    Ok(PublicActionPools {
        observe: crate::query_rpc_pool_for_route(&sensitive, http),
        submit: crate::query_rpc_pool_for_route(&submit, http),
        route: sensitive,
    })
}

/// Each preflight, including a replacement's, quotes fees from its own candidates on the
/// Sensitive route of `pools`, including its nonce, estimate and balance reads. Observation
/// continues on that class and broadcast goes to the Submit pool. Each handed-off attempt joins
/// `activity`'s entry as its step before the raw send.
pub(in crate::public_wallet) async fn submit_public_action_step_session(
    step: PublicActionProgressStep,
    base_tx_req: TransactionRequest,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    signer: &VaultedPublicSigner,
    label: &str,
    pools: &PublicActionPools,
    finality_depth: u64,
    http: &HttpContext,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    activity: Option<(&ActivitySubmission, ActivityStep)>,
    chain_id: u64,
    from_address: Address,
    gas: &EffectiveChainGasSettings,
    funding_budget: Option<PublicShieldFundingBudget>,
    authorized_gas_limit: Option<u64>,
    mut nonce: Option<u64>,
    gas_fee: PublicActionGasFeeSelection,
    fee_policy: PublicActionStepFeePolicy,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    mut handoff: Option<&mut PublicActionHandoff<'_>>,
    end: PublicActionStepEnd,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<PublicActionStepResult> {
    let (query_rpc_pool, submit_pool, rpc_route) =
        (&pools.observe, pools.submit.as_ref(), &pools.route);
    let private_handoff = handoff.is_some();
    if end == PublicActionStepEnd::HandOff && !private_handoff {
        return Err(eyre!(
            "only a durable hand-off ends a public action at its hand-off"
        ));
    }
    let mut railway_auto = fee_policy == PublicActionStepFeePolicy::RefreshRailwayStandard;
    let mut next_gas_fee =
        public_action_step_initial_gas_fee_selection(profile, fee_policy, gas_fee);
    let authorized_gas_fee = authorized_gas_limit.map(|_| gas_fee);
    let authorized_fee_ceiling = railway_auto.then_some(authorized_fee_ceiling).flatten();
    let mut submitted_attempts = Vec::new();
    let mut observer = None;

    loop {
        progress(public_action_progress_update(
            step,
            PublicActionProgressStatus::Pending,
            None,
            None,
        ));

        let preflight = match public_action_preflight_from_rpc_pool(
            query_rpc_pool.as_ref(),
            http,
            rpc_route,
            chain_id,
            from_address,
            base_tx_req.clone(),
            next_gas_fee,
            gas,
            profile,
            asset,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            None,
            authorized_fee_ceiling,
            railway_auto,
            funding_budget,
        )
        .await
        {
            Ok(preflight) => preflight,
            Err(PublicActionPreflightError::FeeAuthorizationRequired {
                max_fee_per_gas,
                max_priority_fee_per_gas,
                message,
            }) => {
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Pending,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::FeeAuthorizationRequired {
                        step,
                        max_fee_per_gas,
                        max_priority_fee_per_gas,
                        message,
                    },
                );
                let Some(command) = recv_public_action_command(command_rx).await else {
                    return Err(eyre!(
                        "Railway fee authorization was required but no review command was received"
                    ));
                };
                railway_auto = false;
                next_gas_fee = command.gas_fee;
                continue;
            }
            Err(error) => {
                let error = error.into_report();
                let message = report_chain_string(&error);
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Error,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::StepFailed { step, message },
                );
                if authorized_gas_limit.is_some() {
                    return Err(error).wrap_err(
                        "advanced transaction requires a refreshed estimate and authorization",
                    );
                }
                // A prepared native shield's reviewed gas and fee bounds cannot be widened by a
                // retry; it needs fresh preparation and review on the same stealth account.
                if matches!(
                    gas_limit_strategy,
                    PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
                ) {
                    return Err(error)
                        .wrap_err("native shield requires refreshed preparation and review");
                }
                let Some(command) = recv_public_action_command(command_rx).await else {
                    return Err(error);
                };
                railway_auto = false;
                next_gas_fee = command.gas_fee;
                continue;
            }
        };
        nonce = Some(preflight.nonce);

        if observer.is_none() {
            let established = BlockObserver::establish(
                Arc::clone(query_rpc_pool),
                finality_depth,
                http.rpc_broker(),
                rpc_route,
            )
            .await?;
            observer = Some(
                crate::public_wallet::PublicTransactionObservationGuard::new(
                    established,
                    transaction_tracking,
                )?,
            );
        }

        if let Some(context) = transaction_tracking {
            context.ensure_open()?;
        }
        emit_public_action_event(event_tx, PublicActionSessionEvent::AttemptHandoff { step });
        let attempted_request = preflight.tx_req.clone();
        let result = submit_public_action_attempt(
            step,
            preflight,
            submit_pool,
            http.network_mode(),
            signer,
            label,
            event_tx,
            None,
            !private_handoff,
            &mut |attempt, signed| {
                if let Some(context) = transaction_tracking {
                    context.ensure_open()?;
                }
                if let Some(handoff) = handoff.as_deref_mut() {
                    handoff(attempt.tx_hash, &attempted_request, signed)?;
                }
                if let Some((activity, activity_step)) = activity {
                    activity.add_step_attempt(attempt.tx_hash, activity_step);
                }
                retain_public_action_attempt(
                    observer
                        .as_mut()
                        .expect("public action observer established"),
                    &mut submitted_attempts,
                    attempt,
                );
                Ok(())
            },
        )
        .await;
        match result {
            Ok(attempt) => progress(public_action_progress_update(
                step,
                PublicActionProgressStatus::Pending,
                Some(attempt.info.tx_hash),
                None,
            )),
            Err(
                PublicActionAttemptError::Signing(error) | PublicActionAttemptError::Sending(error),
            ) => {
                let message = report_chain_string(&error);
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Error,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::StepFailed { step, message },
                );
                // A failed raw-send response can still mean the transaction was accepted.
                // Keep observing every handed-off attempt while accepting explicit replacements.
                if submitted_attempts.is_empty() {
                    let Some(command) = recv_public_action_command(command_rx).await else {
                        return Err(error);
                    };
                    ensure_public_action_command_gas_fee_authorized(
                        authorized_gas_fee,
                        command.gas_fee,
                    )?;
                    railway_auto = false;
                    next_gas_fee = command.gas_fee;
                    continue;
                }
                // No RPC accepted the handed-off attempt. The durable hand-off's owner
                // follows it from here rather than waiting for a receipt that may not come.
                if end == PublicActionStepEnd::HandOff {
                    return Ok(PublicActionStepResult::HandedOff);
                }
            }
        }
        loop {
            let receipt = if command_rx.is_some() {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(3)) => {
                        observer
                            .as_mut()
                            .expect("public action observer established")
                            .poll()
                            .await?
                            .receipt
                    }
                    command = recv_public_action_command(command_rx) => {
                        let Some(command) = command else {
                            *command_rx = None;
                            continue;
                        };
                        if let Err(error) = ensure_public_action_command_gas_fee_authorized(
                            authorized_gas_fee,
                            command.gas_fee,
                        ) {
                            emit_public_action_event(
                                event_tx,
                                PublicActionSessionEvent::AttemptRejected {
                                    step,
                                    message: report_chain_string(&error),
                                },
                            );
                            continue;
                        }
                        let Some(nonce) = nonce else {
                            railway_auto = false;
                            next_gas_fee = command.gas_fee;
                            break;
                        };
                        railway_auto = false;
                        let gas_limit = submitted_attempts
                            .last()
                            .map_or(0, |attempt| attempt.info.gas_limit);
                        let replacement = match public_action_preflight_from_rpc_pool(
                            query_rpc_pool.as_ref(),
                            http,
                            rpc_route,
                            chain_id,
                            from_address,
                            base_tx_req.clone(),
                            command.gas_fee,
                            gas,
                            profile,
                            asset,
                            gas_limit_strategy,
                            authorized_gas_limit,
                            Some(nonce),
                            Some(gas_limit),
                            authorized_fee_ceiling,
                            railway_auto,
                            funding_budget,
                        )
                        .await
                        {
                            Ok(preflight) => preflight,
                            Err(error) => {
                                emit_public_action_event(
                                    event_tx,
                                    PublicActionSessionEvent::AttemptRejected {
                                        step,
                                        message: error.to_string(),
                                    },
                                );
                                continue;
                            }
                        };
                        if let Some(context) = transaction_tracking {
                            context.ensure_open()?;
                        }
                        emit_public_action_event(
                            event_tx,
                            PublicActionSessionEvent::AttemptHandoff { step },
                        );
                        let replacement_request = replacement.tx_req.clone();
                        match submit_public_action_attempt(
                            step,
                            replacement,
                            submit_pool,
                            http.network_mode(),
                            signer,
                            label,
                            event_tx,
                            None,
                            !private_handoff,
                            &mut |attempt, signed| {
                                if let Some(context) = transaction_tracking {
                                    context.ensure_open()?;
                                }
                                // A replacement is handed off as its first attempt was.
                                if let Some(handoff) = handoff.as_deref_mut() {
                                    handoff(attempt.tx_hash, &replacement_request, signed)?;
                                }
                                if let Some((activity, activity_step)) = activity {
                                    activity.add_step_attempt(attempt.tx_hash, activity_step);
                                }
                                retain_public_action_attempt(
                                    observer.as_mut().expect("public action observer established"),
                                    &mut submitted_attempts,
                                    attempt,
                                );
                                Ok(())
                            },
                        )
                        .await
                        {
                            Ok(attempt) => {
                                progress(public_action_progress_update(
                                    step,
                                    PublicActionProgressStatus::Pending,
                                    Some(attempt.info.tx_hash.clone()),
                                    None,
                                ));
                            }
                            Err(error) => emit_public_action_event(
                                event_tx,
                                PublicActionSessionEvent::AttemptRejected {
                                    step,
                                    message: error.message(),
                                },
                            ),
                        }
                        continue;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_secs(3)).await;
                observer
                    .as_mut()
                    .expect("public action observer established")
                    .poll()
                    .await?
                    .receipt
            };

            if let Some((winner_index, receipt)) = receipt {
                let winner = &submitted_attempts[winner_index];
                if !private_handoff {
                    tracing::info!(target: LOG_TARGET,
                        step = ?step,
                        rpc_gas_price = winner.rpc_gas_price,
                        "public action receipt confirmed from submitted attempts"
                    );
                }
                if receipt.status {
                    if end != PublicActionStepEnd::Evidence {
                        progress(public_action_progress_update(
                            step,
                            PublicActionProgressStatus::Done,
                            Some(receipt.tx_hash.clone()),
                            None,
                        ));
                    }
                } else {
                    let message = "Transaction reverted".to_string();
                    progress(public_action_progress_update(
                        step,
                        PublicActionProgressStatus::Error,
                        Some(receipt.tx_hash.clone()),
                        Some(message.clone()),
                    ));
                    emit_public_action_event(
                        event_tx,
                        PublicActionSessionEvent::StepFailed { step, message },
                    );
                    let gas_fee = public_action_winner_gas_fee(&submitted_attempts, winner_index);
                    // A prepared native retry needs fresh preparation and review on the same
                    // stealth account, never a new payer nonce for the same signed payload.
                    let prepared_native = matches!(
                        gas_limit_strategy,
                        PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
                    );
                    let command = if prepared_native {
                        None
                    } else {
                        recv_public_action_command(command_rx).await
                    };
                    let Some(command) = command else {
                        return Ok(PublicActionStepResult::Receipt(PublicActionStepOutcome {
                            receipt,
                            next_nonce: winner.info.nonce.saturating_add(1),
                            gas_fee,
                        }));
                    };
                    ensure_public_action_command_gas_fee_authorized(
                        authorized_gas_fee,
                        command.gas_fee,
                    )?;
                    nonce = Some(winner.info.nonce.saturating_add(1));
                    next_gas_fee = command.gas_fee;
                    submitted_attempts.clear();
                    observer = None;
                    break;
                }
                let gas_fee = public_action_winner_gas_fee(&submitted_attempts, winner_index);
                return Ok(PublicActionStepResult::Receipt(PublicActionStepOutcome {
                    receipt,
                    next_nonce: winner.info.nonce.saturating_add(1),
                    gas_fee,
                }));
            }
        }
    }
}

pub(super) fn retain_public_action_attempt(
    observer: &mut BlockObserver,
    attempts: &mut Vec<SubmittedPublicActionAttempt>,
    attempt: &SubmittedPublicActionAttempt,
) {
    let attempt_id = attempts.len();
    attempts.push(attempt.clone());
    observer.register(attempt.tx_hash, attempt_id);
}
