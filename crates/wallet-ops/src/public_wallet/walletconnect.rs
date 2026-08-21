use eyre::{Result, WrapErr, eyre};

use super::runtime::public_chain_runtime_config;
use super::signer::{VaultedPublicSigner, vaulted_public_signer};
use super::submission::{
    PublicActionPreflightMode, emit_public_action_event,
    emit_refreshed_public_action_hardware_session, public_action_preflight_from_rpc_pool_with_mode,
    sanitize_walletconnect_transaction_request, submit_public_action_attempt,
    validate_walletconnect_reviewed_transaction,
};
use super::types::{
    PublicActionGasLimitStrategy, PublicActionProgressStep, PublicActionSessionEvent,
    PublicShieldTransactionProfile, WalletConnectHardwareTypedDataCapabilityRequest,
    WalletConnectHardwareTypedDataCapabilityResult,
    WalletConnectHardwareTypedDataHashFallbackConfirmationRequired,
    WalletConnectPersonalSignRequest, WalletConnectSendTransactionRequest,
    WalletConnectSendTransactionResult, WalletConnectTypedDataSignRequest,
    is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required,
};
use crate::hardware::HardwareTypedDataSigningMode;
use crate::hardware_typed_data::HardwareEip712Model;
use crate::walletconnect::WalletConnectSupportedMethod;
use crate::{HttpContext, query_rpc_pool_with_http_client, report_chain_string};

pub async fn walletconnect_sign_personal_message(
    request: WalletConnectPersonalSignRequest,
) -> Result<String> {
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        Some(request.vault_password.as_str()),
        &request.public_account_uuid,
        request.protected_software_seed_session.as_deref(),
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )?;
    let event_tx = request.event_tx.as_ref();
    let requires_device_approval = signer.requires_device_approval();
    if requires_device_approval {
        emit_public_action_event(event_tx, PublicActionSessionEvent::HardwareApprovalStarted);
    }
    let signature = match signer.sign_personal_message(&request.message).await {
        Ok(signature) => {
            emit_refreshed_public_action_hardware_session(event_tx, &signer);
            if requires_device_approval {
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::HardwareApprovalCompleted,
                );
            }
            signature
        }
        Err(error) => {
            if requires_device_approval {
                let message = report_chain_string(&error);
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::HardwareApprovalFailed { message },
                );
            }
            return Err(error).wrap_err("WalletConnect personal_sign");
        }
    };
    Ok(alloy::hex::encode_prefixed(signature.as_bytes()))
}

pub async fn walletconnect_sign_typed_data(
    request: WalletConnectTypedDataSignRequest,
    method: WalletConnectSupportedMethod,
) -> Result<String> {
    let operation = match method {
        WalletConnectSupportedMethod::EthSignTypedData
        | WalletConnectSupportedMethod::EthSignTypedDataV4 => method.as_str(),
        _ => {
            return Err(eyre!(
                "WalletConnect {} is not a typed-data signing method",
                method.as_str()
            ));
        }
    };
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        Some(request.vault_password.as_str()),
        &request.public_account_uuid,
        request.protected_software_seed_session.as_deref(),
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )
    .wrap_err_with(|| format!("WalletConnect {operation}"))?;
    let typed_data = HardwareEip712Model::from_walletconnect_typed_data_json(request.typed_data)
        .wrap_err("WalletConnect typed-data payload")
        .wrap_err_with(|| format!("WalletConnect {operation}"))?;
    let event_tx = request.event_tx.as_ref();
    let requires_device_approval = signer.requires_device_approval();
    let hardware_typed_data_mode = signer
        .typed_data_signing_mode()
        .await
        .wrap_err_with(|| format!("WalletConnect {operation}"))?;
    if let Some(mode) = hardware_typed_data_mode {
        if !mode.is_supported() {
            return Err(eyre!(
                "WalletConnect {operation} is unsupported for this hardware Public account session"
            ));
        }
        if mode.requires_hash_fallback_warning() && !request.hash_fallback_confirmed {
            return Err(
                WalletConnectHardwareTypedDataHashFallbackConfirmationRequired::new(
                    signer
                        .refreshed_hardware_session()
                        .wrap_err_with(|| format!("WalletConnect {operation}"))?,
                ),
            )
            .wrap_err_with(|| format!("WalletConnect {operation}"));
        }
    }
    if requires_device_approval {
        emit_public_action_event(event_tx, PublicActionSessionEvent::HardwareApprovalStarted);
    }
    let signature = match signer
        .sign_typed_data_v4(
            &typed_data,
            hardware_typed_data_mode,
            request.hash_fallback_confirmed,
        )
        .await
    {
        Ok(signature) => {
            emit_refreshed_public_action_hardware_session(event_tx, &signer);
            if requires_device_approval {
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::HardwareApprovalCompleted,
                );
            }
            signature
        }
        Err(error) => {
            let error = error.wrap_err(format!("WalletConnect {operation}"));
            let confirmation_required =
                is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(&error);
            if confirmation_required {
                emit_refreshed_public_action_hardware_session(event_tx, &signer);
            } else if requires_device_approval {
                let message = report_chain_string(&error);
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::HardwareApprovalFailed { message },
                );
            }
            return Err(error);
        }
    };
    Ok(alloy::hex::encode_prefixed(signature.as_bytes()))
}

pub async fn walletconnect_sign_typed_data_v4(
    request: WalletConnectTypedDataSignRequest,
) -> Result<String> {
    walletconnect_sign_typed_data(request, WalletConnectSupportedMethod::EthSignTypedDataV4).await
}

pub async fn walletconnect_probe_hardware_typed_data_signing_mode(
    request: WalletConnectHardwareTypedDataCapabilityRequest,
) -> Result<WalletConnectHardwareTypedDataCapabilityResult> {
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        None,
        &request.public_account_uuid,
        None,
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )?;
    let VaultedPublicSigner::Hardware(signer) = &signer else {
        return Ok(WalletConnectHardwareTypedDataCapabilityResult {
            mode: HardwareTypedDataSigningMode::Unsupported,
            refreshed_hardware_session: None,
        });
    };
    let mode = signer
        .typed_data_signing_mode()
        .await
        .wrap_err("probe hardware typed-data capability")?;
    Ok(WalletConnectHardwareTypedDataCapabilityResult {
        mode,
        refreshed_hardware_session: Some(signer.hardware_session()?),
    })
}

pub async fn submit_walletconnect_send_transaction(
    request: WalletConnectSendTransactionRequest,
    http: &HttpContext,
) -> Result<WalletConnectSendTransactionResult> {
    let chain = public_chain_runtime_config(request.chain_id, request.effective_chain.as_ref())?;
    let signer = vaulted_public_signer(
        &request.vault_store,
        &request.view_session,
        Some(request.vault_password.as_str()),
        &request.public_account_uuid,
        request.protected_software_seed_session.as_deref(),
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
    )?;
    let from_address = signer.address();
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let tx_req =
        sanitize_walletconnect_transaction_request(request.tx_req, request.chain_id, from_address);
    let operation_gas_limit = request
        .decoded_transaction
        .as_ref()
        .and_then(|transaction| {
            super::gas::public_walletconnect_operation_gas_limit(
                &transaction.kind,
                chain.gas.gas_limit_buffer,
            )
        });
    let gas_fee = request.reviewed_fee.map_or(request.gas_fee, |fee| {
        super::types::PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: fee.max_fee_per_gas,
            max_priority_fee_per_gas: fee.max_priority_fee_per_gas,
        }
    });
    let reviewed_gas_limit = if operation_gas_limit.is_none() {
        match request.reviewed_transaction {
            Some(reviewed) => {
                validate_walletconnect_reviewed_transaction(
                    request.chain_id,
                    from_address,
                    &tx_req,
                    reviewed.payload_fingerprint,
                    reviewed.gas_limit,
                )?;
                Some(reviewed.gas_limit)
            }
            None => None,
        }
    } else {
        None
    };
    let preflight = public_action_preflight_from_rpc_pool_with_mode(
        &query_rpc_pool,
        http.network_mode(),
        request.chain_id,
        from_address,
        tx_req,
        gas_fee,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicActionGasLimitStrategy::ChainBuffer,
        reviewed_gas_limit,
        None,
        operation_gas_limit,
        PublicActionPreflightMode::Managed,
        None,
        false,
    )
    .await
    .wrap_err("WalletConnect eth_sendTransaction preflight")?;
    emit_public_action_event(
        request.event_tx.as_ref(),
        PublicActionSessionEvent::AttemptHandoff {
            step: PublicActionProgressStep::Send,
        },
    );
    let attempt = submit_public_action_attempt(
        PublicActionProgressStep::Send,
        preflight,
        &query_rpc_pool,
        http.network_mode(),
        &signer,
        "WalletConnect eth_sendTransaction",
        request.event_tx.as_ref(),
        request.expiry_timestamp,
    )
    .await
    .map_err(|error| eyre!(error.message()))?;

    Ok(WalletConnectSendTransactionResult {
        tx_hash: attempt.info.tx_hash,
    })
}
