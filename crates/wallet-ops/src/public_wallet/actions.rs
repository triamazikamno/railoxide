use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};

use super::contracts::{PublicErc20, PublicWrappedNative};
use super::gas::public_advanced_transaction_payload_fingerprint;
use super::native_shield::{
    NativeShieldPayer, NativeShieldPreparation, ensure_native_shield_delivered,
    stealth_native_shield_contracts,
};
use super::runtime::{
    PublicChainRuntimeConfig, public_shield_token, verified_public_chain_runtime_config,
};
use super::signer::{
    VaultedPublicSigner, admitted_gas_payer_signer, admitted_public_signer_authorized,
};
use super::submission::{
    PublicActionHandoff, PublicActionPools, PublicActionStepEnd, PublicShieldFundingBudget,
    emit_public_action_event, emit_refreshed_public_action_hardware_session, public_action_pools,
    public_action_progress_update, recv_public_action_command, submit_public_action_step_session,
};
use super::types::{
    PublicActionCommandReceiver, PublicActionGasFeeMode, PublicActionGasFeeSelection,
    PublicActionGasLimitStrategy, PublicActionProgressStatus, PublicActionProgressStep,
    PublicActionProgressUpdate, PublicActionSessionEvent, PublicActionSessionEventSender,
    PublicActionStepFeePolicy, PublicAdvancedTransactionAuthorization, PublicAssetId,
    PublicSendRequest, PublicSendResult, PublicShieldApprovalMode, PublicShieldRequest,
    PublicShieldTransactionProfile, PublicTransactionIntent,
};
use crate::block_observer::resolve_source_transaction_by_block;
use crate::settings::EffectiveChainConfig;
use crate::vault::DesktopViewSession;
use crate::{
    HttpContext, OperationHttpClient, RpcRoute, ShieldSendOutput, WalletRpcOrigin,
    report_chain_string,
};

pub async fn submit_public_send(
    request: PublicSendRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicSendResult> {
    submit_public_send_with_progress(request, http, client, |_| {}).await
}

pub async fn submit_public_send_with_progress(
    request: PublicSendRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<PublicSendResult> {
    validate_public_transaction_intent(&request.intent)?;
    validate_public_wrapped_native_target(&request.intent, &request.effective_chain)?;
    let (step, operation_label, revert_subject) = match request.intent {
        PublicTransactionIntent::WrapNative { .. } => (
            PublicActionProgressStep::Wrap,
            "public-wrap",
            "public wrap transaction",
        ),
        PublicTransactionIntent::UnwrapNative { .. } => (
            PublicActionProgressStep::Unwrap,
            "public-unwrap",
            "public unwrap transaction",
        ),
        PublicTransactionIntent::Transfer { .. } | PublicTransactionIntent::Raw { .. } => (
            PublicActionProgressStep::Send,
            "public-send",
            "public send transaction",
        ),
    };
    let signer = admitted_public_signer_authorized(
        &request.vault_store,
        &request.view_session,
        request.authorization.as_ref(),
        &request.public_account_uuid,
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
        request.executor_owner.as_ref(),
        request.chain_id,
        client,
    )
    .await?;
    signer
        .while_active(async {
            let mut command_rx = request.command_rx;
            let tx = submit_public_action_step_with_signer(
                step,
                operation_label,
                revert_subject,
                request.chain_id,
                &request.effective_chain,
                &request.intent,
                &signer,
                request.advanced_authorization,
                false,
                request.gas_fee,
                &mut command_rx,
                request.event_tx.as_ref(),
                http,
                client,
                request.transaction_tracking.as_ref(),
                &mut progress,
            )
            .await?;
            Ok(PublicSendResult { tx })
        })
        .await
}

/// Submit one public action with an already-derived signer.  Governance workflows use this
/// narrow entry point to keep one signer session and command channel across ordered calls while
/// allowing every call to run the normal RPC nonce, gas, signing, and confirmation machinery.
pub(crate) async fn submit_public_action_step_with_signer(
    step: PublicActionProgressStep,
    operation_label: &str,
    revert_subject: &str,
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    intent: &PublicTransactionIntent,
    signer: &super::signer::VaultedPublicSigner,
    advanced_authorization: Option<PublicAdvancedTransactionAuthorization>,
    dynamic_gas_preflight: bool,
    gas_fee: PublicActionGasFeeSelection,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    http: &HttpContext,
    client: &OperationHttpClient,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<crate::TxReceiptOutput> {
    validate_public_transaction_intent(intent)?;
    let chain = verified_public_chain_runtime_config(chain_id, effective_chain, http, None).await?;
    let from_address = signer.address();
    let authorized_gas_limit = public_action_authorized_gas_limit(
        chain_id,
        from_address,
        intent,
        advanced_authorization,
        dynamic_gas_preflight,
        gas_fee,
    )?;
    let pools = public_action_pools(&chain.rpc_route, http, client)?;
    let tx_req = public_send_transaction_request(chain_id, from_address, intent)?;
    let tx = submit_public_action_step_session(
        step,
        tx_req,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicShieldTransactionProfile::Railoxide.gas_limit_strategy(),
        signer,
        operation_label,
        &pools,
        chain.finality_depth,
        http,
        transaction_tracking,
        chain_id,
        from_address,
        &chain.gas,
        None,
        authorized_gas_limit,
        None,
        gas_fee,
        PublicActionStepFeePolicy::Custom,
        None,
        command_rx,
        event_tx,
        None,
        PublicActionStepEnd::Receipt,
        progress,
    )
    .await?
    .into_receipt()?
    .receipt;
    if !tx.status {
        return Err(eyre!("{revert_subject} reverted ({})", tx.tx_hash));
    }
    Ok(tx)
}

fn public_action_authorized_gas_limit(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
    authorization: Option<PublicAdvancedTransactionAuthorization>,
    dynamic_gas_preflight: bool,
    gas_fee: PublicActionGasFeeSelection,
) -> Result<Option<u64>> {
    if dynamic_gas_preflight {
        if !matches!(intent, PublicTransactionIntent::Raw { .. }) || authorization.is_some() {
            return Err(eyre!(
                "dynamic public action gas preflight is restricted to an unauthorised raw workflow step"
            ));
        }
        return Ok(None);
    }
    public_send_authorized_gas_limit(chain_id, from, intent, authorization, gas_fee)
}

pub(super) fn public_send_authorized_gas_limit(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
    authorization: Option<super::types::PublicAdvancedTransactionAuthorization>,
    gas_fee: super::types::PublicActionGasFeeSelection,
) -> Result<Option<u64>> {
    match intent {
        PublicTransactionIntent::Transfer { .. }
        | PublicTransactionIntent::WrapNative { .. }
        | PublicTransactionIntent::UnwrapNative { .. } => {
            if authorization.is_some() {
                return Err(eyre!(
                    "advanced transaction authorization cannot be used for a transfer"
                ));
            }
            Ok(None)
        }
        PublicTransactionIntent::Raw { .. } => {
            let authorization =
                authorization.ok_or_else(|| eyre!("advanced transaction estimate is required"))?;
            if authorization.gas_limit == 0 {
                return Err(eyre!(
                    "authorized advanced gas limit must be greater than zero"
                ));
            }
            let super::types::PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } = gas_fee
            else {
                return Err(eyre!(
                    "advanced transaction submission requires the estimated gas fee values"
                ));
            };
            let fingerprint = public_advanced_transaction_payload_fingerprint(
                chain_id,
                from,
                intent,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            );
            if fingerprint != authorization.payload_fingerprint {
                return Err(eyre!(
                    "advanced transaction payload changed after gas estimation"
                ));
            }
            Ok(Some(authorization.gas_limit))
        }
    }
}

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
                super::gas::public_shield_estimated_gas_units(
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
                    super::gas::resolve_public_action_gas_fee(
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

/// A native shield from a stealth account of the selected private wallet, funded and sent by
/// the admitted payer `signer`, in the transaction profile it was prepared for.
async fn submit_stealth_native_shield(
    mut request: PublicShieldRequest,
    preparation: &NativeShieldPreparation,
    signer: VaultedPublicSigner,
    chain: &PublicChainRuntimeConfig,
    pools: &PublicActionPools,
    recipient: &broadcaster_core::crypto::railgun::AddressData,
    http: &HttpContext,
    client: &OperationHttpClient,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<ShieldSendOutput> {
    let mut command_rx = request.command_rx.take();
    let event_tx = request.event_tx.take();
    // Checked before the shield key or any transaction is signed.
    let contracts = stealth_native_shield_contracts(
        request.chain_id,
        &request.effective_chain,
        NativeShieldPayer::of(&signer, request.authorization.as_ref()),
    )?;
    let payer = signer.address();
    preparation.ensure_matches(
        &contracts,
        request.profile,
        payer,
        recipient,
        request.amount,
    )?;
    let owner = request.executor_owner.as_ref().ok_or_else(|| {
        eyre!("Wait for the private wallet's network to load, then shield again.")
    })?;
    let (Some(operation), Some(authorization)) =
        (request.native_operation, request.authorization.as_ref())
    else {
        return Err(eyre!(
            "native shielding needs a fresh preparation and review"
        ));
    };
    let shield_private_key = signer.derive_shield_private_key().await?;
    let shield_request = preparation.shield_request(&shield_private_key)?;
    let purpose_summary = format!(
        "Shield {} → {}",
        request
            .effective_chain
            .native_currency
            .format_amount(request.amount),
        request
            .view_session
            .receive_address()
            .wrap_err("derive selected private wallet receive address")?,
    );
    // An executor-derived payer holds its owner's activity, which issuing takes, so it is
    // released and admitted again to send.
    let readmit = matches!(signer, VaultedPublicSigner::Executor(..));
    let signer = if readmit {
        drop(signer);
        None
    } else {
        Some(signer)
    };
    let issued = owner
        .issue_native_shield(
            operation,
            &request.view_session,
            payer,
            authorization,
            contracts.wrapped_native,
            request.amount,
            shield_request,
            &purpose_summary,
            client,
        )
        .await?;
    let stealth_account = issued
        .transaction()
        .to
        .and_then(|to| to.to().copied())
        .ok_or_else(|| eyre!("the native shield has no stealth account"))?;
    let signer = match signer {
        Some(signer) => signer,
        None => {
            admitted_public_signer_authorized(
                &request.vault_store,
                &request.view_session,
                Some(authorization),
                &request.public_account_uuid,
                None,
                None,
                Some(owner),
                request.chain_id,
                client,
            )
            .await?
        }
    };
    if signer.address() != payer {
        return Err(eyre!("the Public account changed; review the shield again"));
    }
    let mut handoff = owner.native_shield_handoff(&issued)?;
    let handoff: &mut PublicActionHandoff<'_> = &mut handoff;
    let fee_policy = if request.profile == PublicShieldTransactionProfile::Railway
        && request.gas_fee_mode == PublicActionGasFeeMode::Auto
    {
        PublicActionStepFeePolicy::Captured
    } else {
        PublicActionStepFeePolicy::Custom
    };
    let receipt = signer
        .while_active(async {
            Ok(submit_public_action_step_session(
                PublicActionProgressStep::Shield,
                issued.transaction().clone(),
                request.profile,
                request.asset,
                PublicActionGasLimitStrategy::NativeShieldPrepared {
                    reviewed_gas_limit: preparation.gas_limit(),
                    profile: request.profile,
                },
                &signer,
                "public-shield",
                pools,
                chain.finality_depth,
                http,
                request.transaction_tracking.as_ref(),
                request.chain_id,
                payer,
                &chain.gas,
                Some(PublicShieldFundingBudget {
                    gas_units: preparation.gas_limit(),
                    remaining_shield_gas_units: 0,
                    remaining_max_fee_per_gas: None,
                }),
                None,
                None,
                request.gas_fee,
                fee_policy,
                Some(request.authorized_fee_ceiling),
                &mut command_rx,
                event_tx.as_ref(),
                Some(handoff),
                PublicActionStepEnd::Evidence,
                &mut *progress,
            )
            .await?
            .into_receipt()?
            .receipt)
        })
        .await?;
    // Reconciliation below takes the owner's activity.
    drop(signer);
    // The session reported the revert, which returned the value to the payer.
    if !receipt.status {
        return Err(eyre!(
            "Native shield transaction reverted ({}); prepare and review a new shield to try again",
            receipt.tx_hash
        ));
    }
    // Native delivery needs a Shield event, not only the receipt's status.
    let logs = async {
        let hash = receipt
            .tx_hash
            .parse()
            .wrap_err("read the native shield receipt")?;
        let source =
            resolve_source_transaction_by_block(&pools.observe, receipt.block_number, hash).await?;
        Ok::<_, eyre::Report>(source.logs)
    }
    .await;
    if let Err(error) =
        ensure_native_shield_delivered(&contracts, stealth_account, &receipt.tx_hash, logs)
    {
        let error = eyre::Report::new(error);
        let message = report_chain_string(&error);
        progress(public_action_progress_update(
            PublicActionProgressStep::Shield,
            PublicActionProgressStatus::Error,
            Some(receipt.tx_hash.clone()),
            Some(message.clone()),
        ));
        emit_public_action_event(
            event_tx.as_ref(),
            PublicActionSessionEvent::StepFailed {
                step: PublicActionProgressStep::Shield,
                message,
            },
        );
        return Err(error);
    }
    progress(public_action_progress_update(
        PublicActionProgressStep::Shield,
        PublicActionProgressStatus::Done,
        Some(receipt.tx_hash.clone()),
        None,
    ));
    // A settled account needs no attention; a failed write only leaves its row visible.
    let _ = owner.set_hidden(operation, true);
    // The shield is proven; a failed read leaves the account to its next reconciliation.
    let _ = owner.reconcile_account(operation, client).await;
    Ok(ShieldSendOutput {
        wrap: None,
        approve: None,
        shield: receipt,
    })
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

pub(super) const fn public_shield_approval_amount(
    mode: PublicShieldApprovalMode,
    amount: U256,
) -> U256 {
    match mode {
        PublicShieldApprovalMode::UnlimitedWhenShort => U256::MAX,
        PublicShieldApprovalMode::Exact => amount,
    }
}

/// An exact approval is signed for every shield, whatever the allowance.
pub(super) fn public_shield_approval_required(
    mode: PublicShieldApprovalMode,
    allowance: U256,
    amount: U256,
) -> bool {
    match mode {
        PublicShieldApprovalMode::UnlimitedWhenShort => allowance < amount,
        PublicShieldApprovalMode::Exact => true,
    }
}

pub(crate) async fn query_erc20_allowance(
    chain_route: &crate::ClassRoute,
    http: &HttpContext,
    asset: PublicAssetId,
    owner: Address,
    spender: Address,
) -> Result<U256> {
    let PublicAssetId::Erc20(token) = asset else {
        return Err(eyre!("native shield has no ERC-20 allowance"));
    };
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            RpcRoute::from(chain_route.clone()),
            vec![(
                token,
                PublicErc20::allowanceCall { owner, spender }
                    .abi_encode()
                    .into(),
            )],
            WalletRpcOrigin::PublicWallet.into(),
        )
        .await
        .wrap_err("query public shield ERC-20 allowance")?;
    let output = results
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing public shield ERC-20 allowance result"))?
        .wrap_err("query public shield ERC-20 allowance")?;
    PublicErc20::allowanceCall::abi_decode_returns_validate(&output)
        .wrap_err("decode public shield ERC-20 allowance")
}

/// `owner`'s balance of the ERC-20 `token`, read as [`query_erc20_allowance`] reads an
/// allowance.
pub(crate) async fn query_erc20_balance(
    chain_route: &crate::ClassRoute,
    http: &HttpContext,
    token: Address,
    owner: Address,
) -> Result<U256> {
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            RpcRoute::from(chain_route.clone()),
            vec![(
                token,
                PublicErc20::balanceOfCall { account: owner }
                    .abi_encode()
                    .into(),
            )],
            WalletRpcOrigin::PublicWallet.into(),
        )
        .await
        .wrap_err("query ERC-20 balance")?;
    let output = results
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing ERC-20 balance result"))?
        .wrap_err("query ERC-20 balance")?;
    PublicErc20::balanceOfCall::abi_decode_returns_validate(&output)
        .wrap_err("decode ERC-20 balance")
}

pub(super) fn public_send_transaction_request(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
) -> Result<TransactionRequest> {
    validate_public_transaction_intent(intent)?;
    let mut tx_req = TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(from);
    match intent {
        PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount,
            recipient,
        } => {
            tx_req = tx_req.with_to(*recipient).with_value(*amount);
        }
        PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Erc20(token),
            amount,
            recipient,
        } => {
            tx_req = tx_req.with_to(*token).with_input(
                PublicErc20::transferCall {
                    recipient: *recipient,
                    amount: *amount,
                }
                .abi_encode(),
            );
        }
        PublicTransactionIntent::Raw { to, value, data } => {
            match to {
                Some(to) => tx_req = tx_req.with_to(*to),
                None => tx_req = tx_req.create(),
            }
            tx_req = tx_req.with_value(*value).with_input(data.clone());
        }
        PublicTransactionIntent::WrapNative { wrapped, amount } => {
            tx_req = tx_req
                .with_to(*wrapped)
                .with_value(*amount)
                .with_input(PublicWrappedNative::depositCall {}.abi_encode());
        }
        PublicTransactionIntent::UnwrapNative { wrapped, amount } => {
            tx_req = tx_req
                .with_to(*wrapped)
                .with_input(PublicWrappedNative::withdrawCall { amount: *amount }.abi_encode());
        }
    }
    Ok(tx_req)
}

/// A wrap or unwrap must call the chain's current wrapped native token, so a stale form can
/// never send to a contract the chain no longer names.
fn validate_public_wrapped_native_target(
    intent: &PublicTransactionIntent,
    effective_chain: &EffectiveChainConfig,
) -> Result<()> {
    let (PublicTransactionIntent::WrapNative { wrapped, .. }
    | PublicTransactionIntent::UnwrapNative { wrapped, .. }) = intent
    else {
        return Ok(());
    };
    match effective_chain.wrapped_native_token {
        Some(configured) if configured == *wrapped => Ok(()),
        Some(_) => Err(eyre!(
            "wrapped native token changed; reopen the form to use the chain's current token"
        )),
        None => Err(eyre!("chain has no wrapped native token configured")),
    }
}

pub(super) fn validate_public_transaction_intent(intent: &PublicTransactionIntent) -> Result<()> {
    match intent {
        PublicTransactionIntent::Transfer { amount, .. }
        | PublicTransactionIntent::WrapNative { amount, .. }
        | PublicTransactionIntent::UnwrapNative { amount, .. }
            if amount.is_zero() =>
        {
            Err(eyre!("amount is required"))
        }
        PublicTransactionIntent::Raw { to: None, data, .. } if data.is_empty() => {
            Err(eyre!("contract creation requires non-empty init code"))
        }
        PublicTransactionIntent::Raw {
            to: Some(_),
            value,
            data,
        } if value.is_zero() && data.is_empty() => {
            Err(eyre!("contract call must include native value or data"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Bytes;

    use super::*;

    #[test]
    fn dynamic_gas_preflight_is_workflow_only() {
        let intent = PublicTransactionIntent::Raw {
            to: Some(Address::ZERO),
            value: U256::ZERO,
            data: Bytes::from(vec![1_u8]),
        };
        let fee = PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        };
        assert!(
            public_action_authorized_gas_limit(1, Address::ZERO, &intent, None, false, fee)
                .is_err()
        );
        assert_eq!(
            public_action_authorized_gas_limit(1, Address::ZERO, &intent, None, true, fee).unwrap(),
            None
        );
        let transfer = PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(1_u8),
            recipient: Address::ZERO,
        };
        assert!(
            public_action_authorized_gas_limit(1, Address::ZERO, &transfer, None, true, fee)
                .is_err()
        );
    }
}
