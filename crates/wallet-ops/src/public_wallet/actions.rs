use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider as _;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};

use super::contracts::{PublicErc20, PublicRelayAdapt, RelayAdaptCall};
use super::gas::public_advanced_transaction_payload_fingerprint;
use super::runtime::{public_chain_runtime_config, public_shield_token};
use super::signer::vaulted_public_signer;
use super::submission::{
    emit_public_action_event, emit_refreshed_public_action_hardware_session,
    public_action_progress_update, recv_public_action_command, submit_public_action_step_session,
};
use super::types::{
    PublicActionCommandReceiver, PublicActionGasFeeMode, PublicActionGasFeeSelection,
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent, PublicActionSessionEventSender, PublicActionStepFeePolicy,
    PublicAdvancedTransactionAuthorization, PublicAssetId, PublicSendRequest, PublicSendResult,
    PublicShieldRequest, PublicShieldTransactionProfile, PublicTransactionIntent,
};
use crate::settings::EffectiveChainConfig;
use crate::{HttpContext, ShieldSendOutput, query_rpc_pool_with_http_client, report_chain_string};

pub async fn submit_public_send(
    request: PublicSendRequest,
    http: &HttpContext,
) -> Result<PublicSendResult> {
    submit_public_send_with_progress(request, http, |_| {}).await
}

pub async fn submit_public_send_with_progress(
    request: PublicSendRequest,
    http: &HttpContext,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<PublicSendResult> {
    validate_public_transaction_intent(&request.intent)?;
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        Some(request.vault_password.as_str()),
        &request.public_account_uuid,
        request.protected_software_seed_session.as_deref(),
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )?;
    let mut command_rx = request.command_rx;
    let tx = submit_public_action_step_with_signer(
        PublicActionProgressStep::Send,
        "public-send",
        "public send transaction",
        request.chain_id,
        request.effective_chain.as_ref(),
        &request.intent,
        &signer,
        request.advanced_authorization,
        false,
        request.gas_fee,
        &mut command_rx,
        request.event_tx.as_ref(),
        http,
        &mut progress,
    )
    .await?;
    Ok(PublicSendResult { tx })
}

/// Submit one public action with an already-derived signer.  Governance workflows use this
/// narrow entry point to keep one signer session and command channel across ordered calls while
/// allowing every call to run the normal RPC nonce, gas, signing, and confirmation machinery.
pub(crate) async fn submit_public_action_step_with_signer(
    step: PublicActionProgressStep,
    operation_label: &str,
    revert_subject: &str,
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    intent: &PublicTransactionIntent,
    signer: &super::signer::VaultedPublicSigner,
    advanced_authorization: Option<PublicAdvancedTransactionAuthorization>,
    dynamic_gas_preflight: bool,
    gas_fee: PublicActionGasFeeSelection,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    http: &HttpContext,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<crate::TxReceiptOutput> {
    validate_public_transaction_intent(intent)?;
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let from_address = signer.address();
    let authorized_gas_limit = public_action_authorized_gas_limit(
        chain_id,
        from_address,
        intent,
        advanced_authorization,
        dynamic_gas_preflight,
        gas_fee,
    )?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let tx_req = public_send_transaction_request(chain_id, from_address, intent)?;
    let tx = submit_public_action_step_session(
        step,
        tx_req,
        PublicShieldTransactionProfile::Railoxide,
        PublicShieldTransactionProfile::Railoxide.gas_limit_strategy(PublicAssetId::Native),
        signer,
        operation_label,
        query_rpc_pool,
        chain.finality_depth,
        http.network_mode(),
        chain_id,
        from_address,
        &chain.gas,
        authorized_gas_limit,
        None,
        gas_fee,
        PublicActionStepFeePolicy::Custom,
        None,
        command_rx,
        event_tx,
        progress,
    )
    .await?
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
        PublicTransactionIntent::Transfer { .. } => {
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
) -> Result<ShieldSendOutput> {
    submit_public_shield_with_progress(request, http, |_| {}).await
}

pub async fn submit_public_shield_with_progress(
    request: PublicShieldRequest,
    http: &HttpContext,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<ShieldSendOutput> {
    if request.amount.is_zero() {
        return Err(eyre!("amount is required"));
    }
    let chain = public_chain_runtime_config(request.chain_id, request.effective_chain.as_ref())?;
    let token = public_shield_token(request.asset, &chain)?;
    let recipient = request
        .view_session
        .receive_address()
        .wrap_err("derive selected private wallet receive address")?;
    let railgun_addr = broadcaster_core::crypto::railgun::Address::from(recipient.as_str());
    let addr_data = broadcaster_core::crypto::railgun::AddressData::try_from(&railgun_addr)
        .wrap_err("invalid selected private wallet receive address")?;
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        Some(request.vault_password.as_str()),
        &request.public_account_uuid,
        request.protected_software_seed_session.as_deref(),
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )?;
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
                    let Some(command) = recv_public_action_command(&mut command_rx).await else {
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
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);

    let approval_required = if request.asset == PublicAssetId::Native {
        false
    } else if request.profile == PublicShieldTransactionProfile::Railoxide {
        true
    } else {
        progress(public_action_progress_update(
            PublicActionProgressStep::Approve,
            PublicActionProgressStatus::Pending,
            None,
            None,
        ));
        match query_erc20_allowance(
            &query_rpc_pool,
            request.asset,
            from_address,
            chain.railgun_contract,
        )
        .await
        {
            Ok(allowance) => {
                public_shield_approval_required(request.profile, allowance, request.amount)
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
                return Err(error).wrap_err("check public shield ERC-20 allowance");
            }
        }
    };

    let approve_receipt = if request.asset == PublicAssetId::Native {
        None
    } else if !approval_required {
        progress(public_action_progress_update(
            PublicActionProgressStep::Approve,
            PublicActionProgressStatus::Done,
            None,
            Some("Existing allowance is sufficient".to_string()),
        ));
        None
    } else {
        let approval_amount = public_shield_approval_amount(request.profile, request.amount);
        let approve_data = broadcaster_core::contracts::shield::build_approve_calldata(
            chain.railgun_contract,
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
            request.profile.gas_limit_strategy(request.asset),
            &signer,
            "public-shield-approve",
            query_rpc_pool.clone(),
            chain.finality_depth,
            http.network_mode(),
            request.chain_id,
            from_address,
            &chain.gas,
            None,
            nonce,
            gas_fee,
            fee_policy,
            Some(request.authorized_fee_ceiling),
            &mut command_rx,
            event_tx.as_ref(),
            &mut progress,
        )
        .await?;
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
    };

    let shield_tx = if request.asset == PublicAssetId::Native {
        public_native_shield_transaction_request(
            request.chain_id,
            from_address,
            chain.relay_adapt_contract,
            request.amount,
            shield_data,
        )
    } else {
        TransactionRequest::default()
            .with_chain_id(request.chain_id)
            .with_from(from_address)
            .with_to(chain.railgun_contract)
            .with_input(shield_data)
            .with_nonce(0)
    };
    let shield_receipt = submit_public_action_step_session(
        PublicActionProgressStep::Shield,
        shield_tx,
        request.profile,
        request.profile.gas_limit_strategy(request.asset),
        &signer,
        "public-shield",
        query_rpc_pool,
        chain.finality_depth,
        http.network_mode(),
        request.chain_id,
        from_address,
        &chain.gas,
        None,
        nonce,
        gas_fee,
        fee_policy,
        Some(request.authorized_fee_ceiling),
        &mut command_rx,
        event_tx.as_ref(),
        &mut progress,
    )
    .await?
    .receipt;
    if !shield_receipt.status {
        return Err(eyre!(
            "public shield transaction reverted ({})",
            shield_receipt.tx_hash
        ));
    }

    Ok(ShieldSendOutput {
        wrap: None,
        approve: approve_receipt,
        shield: shield_receipt,
    })
}

pub(super) const fn public_shield_approval_amount(
    profile: PublicShieldTransactionProfile,
    amount: U256,
) -> U256 {
    match profile {
        PublicShieldTransactionProfile::Railway => U256::MAX,
        PublicShieldTransactionProfile::Railoxide => amount,
    }
}

pub(super) fn public_shield_approval_required(
    profile: PublicShieldTransactionProfile,
    allowance: U256,
    amount: U256,
) -> bool {
    match profile {
        PublicShieldTransactionProfile::Railway => allowance < amount,
        PublicShieldTransactionProfile::Railoxide => true,
    }
}

async fn query_erc20_allowance(
    query_rpc_pool: &broadcaster_core::query_rpc_pool::QueryRpcPool,
    asset: PublicAssetId,
    owner: Address,
    spender: Address,
) -> Result<U256> {
    let PublicAssetId::Erc20(token) = asset else {
        return Err(eyre!("native shield has no ERC-20 allowance"));
    };
    let call = TransactionRequest::default()
        .with_to(token)
        .with_input(PublicErc20::allowanceCall { owner, spender }.abi_encode());
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match provider_handle.provider.call(call.clone()).await {
            Ok(output) => {
                return PublicErc20::allowanceCall::abi_decode_returns_validate(&output)
                    .wrap_err("decode public shield ERC-20 allowance");
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.map_or_else(
        || eyre!("no healthy query RPC available"),
        |error| eyre!(error),
    ))
    .wrap_err("query public shield ERC-20 allowance")
}

pub(super) fn public_native_shield_transaction_request(
    chain_id: u64,
    from: Address,
    relay_adapt: Address,
    amount: U256,
    shield_data: Vec<u8>,
) -> TransactionRequest {
    let calls = vec![
        RelayAdaptCall {
            to: relay_adapt,
            data: PublicRelayAdapt::wrapBaseCall { _amount: amount }
                .abi_encode()
                .into(),
            value: U256::ZERO,
        },
        RelayAdaptCall {
            to: relay_adapt,
            data: Bytes::from(shield_data),
            value: U256::ZERO,
        },
    ];
    TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(from)
        .with_to(relay_adapt)
        .with_input(
            PublicRelayAdapt::multicallCall {
                _requireSuccess: true,
                _calls: calls,
            }
            .abi_encode(),
        )
        .with_value(amount)
        .with_nonce(0)
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
    }
    Ok(tx_req)
}

pub(super) fn validate_public_transaction_intent(intent: &PublicTransactionIntent) -> Result<()> {
    match intent {
        PublicTransactionIntent::Transfer { amount, .. } if amount.is_zero() => {
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
