#[allow(clippy::wildcard_imports)]
use super::*;

pub async fn submit_public_shield(
    request: PublicShieldRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<ShieldSendOutput> {
    submit_public_shield_with_progress(request, http, client, |_| {}).await
}

pub async fn submit_public_shield_with_progress(
    mut request: PublicShieldRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<ShieldSendOutput> {
    if request.amount.is_zero() {
        return Err(eyre!("amount is required"));
    }
    request.effective_chain.require_railgun()?;
    let native_prepared = request.native_preparation.is_some();
    // A native shield without a preparation wraps first, which only Railoxide does. A
    // reviewed route is never switched here.
    let wrap_fallback = request.asset == PublicAssetId::Native && !native_prepared;
    if (native_prepared && request.asset != PublicAssetId::Native)
        || (wrap_fallback
            && (request.profile != PublicShieldTransactionProfile::Railoxide
                || request.effective_chain.wrapped_native_token.is_none()))
    {
        return Err(eyre!(
            "native shielding needs a fresh preparation and review"
        ));
    }
    // Wrap, approval and shield attempts share one entry, which never names the receiving wallet.
    let activity = ActivitySubmission::public(
        &request.vault_store,
        &request.view_session,
        PublicActivity {
            account_uuid: request.public_account_uuid.clone(),
            chain_id: request.chain_id,
            kind: ActivityKind::Shield,
            legs: vec![activity_leg(
                activity_asset(request.asset),
                request.amount,
                ActivityDirection::Out,
            )],
            counterparty: None,
        },
    )
    .with_gateway_origin(request.gateway_origin.clone());
    let chain = verified_public_chain_runtime_config(
        request.chain_id,
        &request.effective_chain,
        http,
        None,
    )
    .await?;
    let pools = public_action_pools(&chain.rpc_route, http, client)?;
    let token = public_shield_token(request.asset, &chain)?;
    let addr_data = public_shield_recipient(&request.view_session)?;
    let signer = if native_prepared
        && let Some(authorization @ crate::DesktopPrivateSpendAuthorization::HardwareExecutor(_)) =
            request.authorization.as_ref()
    {
        // A Ledger or Trezor wallet approves the stealth account's execution with its payer.
        admitted_gas_payer_signer(
            &request.vault_store,
            &request.view_session,
            authorization,
            &request.public_account_uuid,
            request.executor_owner.as_ref(),
            request.chain_id,
            client,
        )
        .await?
    } else {
        admitted_public_signer_authorized(
            &request.vault_store,
            &request.view_session,
            request.authorization.as_ref(),
            &request.public_account_uuid,
            request.trezor_app_passphrase.take(),
            request.trezor_pin_matrix_provider.take(),
            request.executor_owner.as_ref(),
            request.chain_id,
            client,
        )
        .await?
    };
    if let Some(preparation) = request.native_preparation.clone() {
        return Box::pin(submit_stealth_native_shield(
            request,
            &preparation,
            signer,
            &chain,
            &pools,
            &addr_data,
            &activity,
            http,
            client,
            &mut progress,
        ))
        .await;
    }
    // The wrap fallback shields the wrapped token it deposits as an ERC-20.
    let shield_asset = PublicAssetId::Erc20(token);
    let approval_mode = request.profile.approval_mode(request.approval_mode);
    signer
        .while_active(async {
            let mut nonce = None;
            let mut gas_fee = request.gas_fee;
            let mut fee_policy = if request.profile == PublicShieldTransactionProfile::Railway
                && request.gas_fee_mode == PublicActionGasFeeMode::Auto
            {
                PublicActionStepFeePolicy::Captured
            } else {
                PublicActionStepFeePolicy::Custom
            };
            let mut command_rx = request.command_rx;
            let event_tx = request.event_tx;
            let shield_private_key = if signer.requires_device_approval() {
                loop {
                    progress(public_action_progress_update(
                        PublicActionProgressStep::ShieldKey,
                        PublicActionProgressStatus::Pending,
                        None,
                        None,
                    ));
                    match signer.derive_shield_private_key().await {
                        Ok(shield_private_key) => {
                            progress(public_action_progress_update(
                                PublicActionProgressStep::ShieldKey,
                                PublicActionProgressStatus::Done,
                                None,
                                None,
                            ));
                            break shield_private_key;
                        }
                        Err(error) => {
                            let message = report_chain_string(&error);
                            progress(public_action_progress_update(
                                PublicActionProgressStep::ShieldKey,
                                PublicActionProgressStatus::Error,
                                None,
                                Some(message.clone()),
                            ));
                            emit_public_action_event(
                                event_tx.as_ref(),
                                PublicActionSessionEvent::StepFailed {
                                    step: PublicActionProgressStep::ShieldKey,
                                    message,
                                },
                            );
                            let Some(command) = recv_public_action_command(&mut command_rx).await
                            else {
                                return Err(error);
                            };
                            gas_fee = command.gas_fee;
                            fee_policy = PublicActionStepFeePolicy::Custom;
                        }
                    }
                }
            } else {
                signer.derive_shield_private_key().await?
            };
            emit_refreshed_public_action_hardware_session(event_tx.as_ref(), &signer);
            let shield_data = broadcaster_core::contracts::shield::build_shield_calldata(
                addr_data.master_public_key,
                &addr_data.viewing_public_key,
                token,
                request.amount,
                &shield_private_key,
            )
            .wrap_err("build public shield calldata")?;

            let from_address = signer.address();

            let (wrap_gas_units, approval_gas_units, shield_gas_units) =
                crate::public_wallet::gas::public_shield_estimated_gas_units(
                    railgun_wallet::tx::RailgunGasModel::for_chain(request.chain_id),
                    request.asset,
                    request.profile,
                    chain.gas.gas_limit_buffer,
                )
                .ok_or_else(|| eyre!("public shield has no gas model"))?;
            let wrap_receipt = if wrap_fallback {
                let wrap_tx = TransactionRequest::default()
                    .with_chain_id(request.chain_id)
                    .with_from(from_address)
                    .with_to(token)
                    .with_input(PublicWrappedNative::depositCall {}.abi_encode())
                    .with_value(request.amount)
                    .with_nonce(0);
                let wrap_outcome = submit_public_action_step_session(
                    PublicActionProgressStep::Wrap,
                    wrap_tx,
                    request.profile,
                    request.asset,
                    request.profile.gas_limit_strategy(),
                    &signer,
                    "public-shield-wrap",
                    &pools,
                    chain.finality_depth,
                    http,
                    request.transaction_tracking.as_ref(),
                    Some((&activity, ActivityStep::Wrap)),
                    request.chain_id,
                    from_address,
                    &chain.gas,
                    Some(PublicShieldFundingBudget {
                        gas_units: wrap_gas_units,
                        remaining_shield_gas_units: approval_gas_units
                            .saturating_add(shield_gas_units),
                        remaining_max_fee_per_gas: None,
                    }),
                    None,
                    nonce,
                    gas_fee,
                    fee_policy,
                    Some(request.authorized_fee_ceiling),
                    &mut command_rx,
                    event_tx.as_ref(),
                    None,
                    PublicActionStepEnd::Receipt,
                    &mut progress,
                )
                .await?
                .into_receipt()?;
                let receipt = wrap_outcome.receipt;
                if !receipt.status {
                    return Err(eyre!(
                        "public shield wrap transaction reverted ({})",
                        receipt.tx_hash
                    ));
                }
                // Only Railoxide wraps, so the later steps carry this step's fee.
                nonce = Some(wrap_outcome.next_nonce);
                gas_fee = wrap_outcome.gas_fee;
                fee_policy = PublicActionStepFeePolicy::Custom;
                Some(receipt)
            } else {
                None
            };

            let approval_required = if approval_mode == PublicShieldApprovalMode::Exact {
                true
            } else {
                loop {
                    progress(public_action_progress_update(
                        PublicActionProgressStep::Approve,
                        PublicActionProgressStatus::Pending,
                        None,
                        None,
                    ));
                    match query_erc20_allowance(
                        &pools.route,
                        http,
                        shield_asset,
                        from_address,
                        chain.require_railgun()?.contract,
                    )
                    .await
                    {
                        Ok(allowance) => {
                            break public_shield_approval_required(
                                approval_mode,
                                allowance,
                                request.amount,
                            );
                        }
                        Err(error) => {
                            let message = report_chain_string(&error);
                            progress(public_action_progress_update(
                                PublicActionProgressStep::Approve,
                                PublicActionProgressStatus::Error,
                                None,
                                Some(message.clone()),
                            ));
                            emit_public_action_event(
                                event_tx.as_ref(),
                                PublicActionSessionEvent::StepFailed {
                                    step: PublicActionProgressStep::Approve,
                                    message,
                                },
                            );
                            let Some(command) = recv_public_action_command(&mut command_rx).await
                            else {
                                return Err(error).wrap_err("check public shield ERC-20 allowance");
                            };
                            // An unchanged fee keeps Mimic Auto's captured fee or the Wrap fee.
                            if command.gas_fee != gas_fee {
                                gas_fee = command.gas_fee;
                                fee_policy = PublicActionStepFeePolicy::Custom;
                            }
                        }
                    }
                }
            };

            let remaining_max_fee_per_gas = if approval_required
                && request.profile == PublicShieldTransactionProfile::Railway
                && request.gas_fee_mode == PublicActionGasFeeMode::Auto
            {
                Some(
                    crate::public_wallet::gas::resolve_public_action_gas_fee(
                        request.chain_id,
                        request.profile,
                        shield_asset,
                        request.authorized_fee_ceiling,
                        None,
                    )?
                    .max_fee_per_gas,
                )
            } else {
                None
            };
            let approve_receipt = if approval_required {
                let approval_amount = public_shield_approval_amount(approval_mode, request.amount);
                let approve_data = broadcaster_core::contracts::shield::build_approve_calldata(
                    chain.require_railgun()?.contract,
                    approval_amount,
                );
                let approve_tx = TransactionRequest::default()
                    .with_chain_id(request.chain_id)
                    .with_from(from_address)
                    .with_to(token)
                    .with_input(approve_data)
                    .with_nonce(0);
                let approve_outcome = submit_public_action_step_session(
                    PublicActionProgressStep::Approve,
                    approve_tx,
                    request.profile,
                    shield_asset,
                    request.profile.gas_limit_strategy(),
                    &signer,
                    "public-shield-approve",
                    &pools,
                    chain.finality_depth,
                    http,
                    request.transaction_tracking.as_ref(),
                    Some((&activity, ActivityStep::Approve)),
                    request.chain_id,
                    from_address,
                    &chain.gas,
                    Some(PublicShieldFundingBudget {
                        gas_units: approval_gas_units,
                        remaining_shield_gas_units: shield_gas_units,
                        remaining_max_fee_per_gas,
                    }),
                    None,
                    nonce,
                    gas_fee,
                    fee_policy,
                    Some(request.authorized_fee_ceiling),
                    &mut command_rx,
                    event_tx.as_ref(),
                    None,
                    PublicActionStepEnd::Receipt,
                    &mut progress,
                )
                .await?
                .into_receipt()?;
                let receipt = approve_outcome.receipt;
                if !receipt.status {
                    return Err(eyre!(
                        "public shield approve transaction reverted ({})",
                        receipt.tx_hash
                    ));
                }
                nonce = Some(approve_outcome.next_nonce);
                if fee_policy == PublicActionStepFeePolicy::Captured
                    && request.profile == PublicShieldTransactionProfile::Railway
                    && request.gas_fee_mode == PublicActionGasFeeMode::Auto
                {
                    fee_policy = PublicActionStepFeePolicy::RefreshRailwayStandard;
                } else {
                    gas_fee = approve_outcome.gas_fee;
                    fee_policy = PublicActionStepFeePolicy::Custom;
                }
                Some(receipt)
            } else {
                progress(public_action_progress_update(
                    PublicActionProgressStep::Approve,
                    PublicActionProgressStatus::Done,
                    None,
                    Some("Existing allowance is sufficient".to_string()),
                ));
                None
            };

            let shield_tx = TransactionRequest::default()
                .with_chain_id(request.chain_id)
                .with_from(from_address)
                .with_to(chain.require_railgun()?.contract)
                .with_input(shield_data)
                .with_nonce(0);
            let shield_receipt = submit_public_action_step_session(
                PublicActionProgressStep::Shield,
                shield_tx,
                request.profile,
                shield_asset,
                request.profile.gas_limit_strategy(),
                &signer,
                "public-shield",
                &pools,
                chain.finality_depth,
                http,
                request.transaction_tracking.as_ref(),
                Some((&activity, ActivityStep::Main)),
                request.chain_id,
                from_address,
                &chain.gas,
                Some(PublicShieldFundingBudget {
                    gas_units: shield_gas_units,
                    remaining_shield_gas_units: 0,
                    remaining_max_fee_per_gas: None,
                }),
                None,
                nonce,
                gas_fee,
                fee_policy,
                Some(request.authorized_fee_ceiling),
                &mut command_rx,
                event_tx.as_ref(),
                None,
                PublicActionStepEnd::Receipt,
                &mut progress,
            )
            .await?
            .into_receipt()?
            .receipt;
            if !shield_receipt.status {
                return Err(eyre!(
                    "public shield transaction reverted ({})",
                    shield_receipt.tx_hash
                ));
            }

            Ok(ShieldSendOutput {
                wrap: wrap_receipt,
                approve: approve_receipt,
                shield: shield_receipt,
            })
        })
        .await
}

/// The selected private wallet's receive address, which a Public Shield pays.
pub fn public_shield_recipient(
    view_session: &DesktopViewSession,
) -> Result<broadcaster_core::crypto::railgun::AddressData> {
    let recipient = view_session
        .receive_address()
        .wrap_err("derive selected private wallet receive address")?;
    let railgun_addr = broadcaster_core::crypto::railgun::Address::from(recipient.as_str());
    broadcaster_core::crypto::railgun::AddressData::try_from(&railgun_addr)
        .wrap_err("invalid selected private wallet receive address")
}

pub(in crate::public_wallet) const fn public_shield_approval_amount(
    mode: PublicShieldApprovalMode,
    amount: U256,
) -> U256 {
    match mode {
        PublicShieldApprovalMode::UnlimitedWhenShort => U256::MAX,
        PublicShieldApprovalMode::Exact => amount,
    }
}

/// An exact approval is signed for every shield, whatever the allowance.
pub(in crate::public_wallet) fn public_shield_approval_required(
    mode: PublicShieldApprovalMode,
    allowance: U256,
    amount: U256,
) -> bool {
    match mode {
        PublicShieldApprovalMode::UnlimitedWhenShort => allowance < amount,
        PublicShieldApprovalMode::Exact => true,
    }
}
