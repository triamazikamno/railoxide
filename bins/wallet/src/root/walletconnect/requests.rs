use super::fee::WalletConnectReviewedFeeProjection;
use super::{
    helpers::{
        current_unix_seconds, parse_caip2_chain_id, walletconnect_await_before_request_expiry,
        walletconnect_pending_request_expired,
    },
    intent::{
        WalletConnectHeroSummary, WalletConnectIntentAction, WalletConnectIntentView,
        WalletConnectPartyRole, WalletConnectRisk, walletconnect_party_role_label,
        walletconnect_selected_account_provenance_visible,
    },
    relay::{publish_walletconnect_session_response, publish_walletconnect_session_response_ref},
    render::chain_label_for_caip2,
    *,
};
use crate::root::tokens::format_native_token_amount_for_display;

pub(super) async fn approve_walletconnect_request_task(
    request: WalletConnectRequestUi,
    vault_store: Arc<DesktopVaultStore>,
    view_session: Arc<DesktopViewSession>,
    vault_password: Zeroizing<String>,
    protected_software_seed_session: Option<Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>>,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    effective_chain: Option<EffectiveChainConfig>,
    context: WalletConnectClientContext,
    http: HttpContext,
    hash_fallback_confirmed: bool,
    reviewed_fee: Option<super::fee::WalletConnectReviewedFeeProjection>,
    event_tx: Option<PublicActionSessionEventSender>,
) -> Result<WalletConnectRequestApprovalOutcome, String> {
    let expiry_timestamp = request.item.expiry_timestamp;
    let use_expiry_timeout = walletconnect_request_approval_uses_expiry_timeout(&request.parsed);
    let response_request = request.clone();
    let authorization = async move {
        let mut submitted_tx_hash = None;
        let request_method = request.parsed.method();
        let result = match request.parsed.clone() {
            WalletConnectParsedRequest::PersonalSign { message, .. } => {
                walletconnect_sign_personal_message(WalletConnectPersonalSignRequest {
                    view_session,
                    vault_store,
                    vault_password,
                    protected_software_seed_session,
                    trezor_app_passphrase,
                    trezor_pin_matrix_provider,
                    public_account_uuid: request.session.selected_public_account_uuid.clone(),
                    message: walletconnect_personal_message_bytes(&message),
                    event_tx,
                })
                .await
                .map(Value::String)
            }
            WalletConnectParsedRequest::EthSignTypedData { typed_data, .. }
            | WalletConnectParsedRequest::EthSignTypedDataV4 { typed_data, .. } => {
                walletconnect_sign_typed_data(
                    WalletConnectTypedDataSignRequest {
                        view_session,
                        vault_store,
                        vault_password,
                        protected_software_seed_session,
                        trezor_app_passphrase,
                        trezor_pin_matrix_provider,
                        public_account_uuid: request.session.selected_public_account_uuid.clone(),
                        typed_data,
                        hash_fallback_confirmed,
                        event_tx,
                    },
                    request_method,
                )
                .await
                .map(Value::String)
            }
            WalletConnectParsedRequest::EthSendTransaction { transaction } => {
                let Some(reviewed_fee) = reviewed_fee.as_ref() else {
                    return (
                        Err(eyre::eyre!(
                            "WalletConnect transaction approval is missing current fee review."
                        )),
                        submitted_tx_hash,
                    );
                };
                let Some(chain_id) = parse_caip2_chain_id(&request.item.chain_id) else {
                    return (
                        Err(eyre::eyre!("WalletConnect request chain is not EIP-155")),
                        submitted_tx_hash,
                    );
                };
                match transaction_request_from_walletconnect(chain_id, transaction) {
                    Ok(tx_req) => submit_walletconnect_send_transaction(
                        WalletConnectSendTransactionRequest {
                            chain_id,
                            effective_chain,
                            view_session,
                            vault_store,
                            vault_password,
                            protected_software_seed_session,
                            trezor_app_passphrase,
                            trezor_pin_matrix_provider,
                            public_account_uuid: request
                                .session
                                .selected_public_account_uuid
                                .clone(),
                            tx_req,
                            decoded_transaction: request.item.decoded_transaction.clone(),
                            reviewed_transaction: reviewed_fee.reviewed_transaction(),
                            reviewed_fee: reviewed_fee.wallet_ops_fee(),
                            gas_fee: reviewed_fee.selection,
                            expiry_timestamp,
                            event_tx,
                        },
                        &http,
                    )
                    .await
                    .map(|result| {
                        submitted_tx_hash = Some(result.tx_hash.clone());
                        Value::String(result.tx_hash)
                    }),
                    Err(error) => Err(error),
                }
            }
            WalletConnectParsedRequest::EthAccounts
            | WalletConnectParsedRequest::EthRequestAccounts
            | WalletConnectParsedRequest::WalletSwitchEthereumChain { .. } => Err(eyre::eyre!(
                "WalletConnect request does not require approval"
            )),
        };
        (result, submitted_tx_hash)
    };
    let (result, submitted_tx_hash) = if use_expiry_timeout {
        let Ok(result) = Box::pin(walletconnect_await_before_request_expiry(
            expiry_timestamp,
            authorization,
        ))
        .await
        else {
            let relay_error =
                publish_walletconnect_expired_request_response(&context, &response_request)
                    .await
                    .err();
            return Ok(WalletConnectRequestApprovalOutcome::expired(
                relay_error.is_none(),
                relay_error,
                None,
            ));
        };
        result
    } else {
        authorization.await
    };
    if walletconnect_approval_should_publish_expired_response(
        expiry_timestamp,
        current_unix_seconds(),
        submitted_tx_hash.as_deref(),
    ) {
        let relay_error =
            publish_walletconnect_expired_request_response(&context, &response_request)
                .await
                .err();
        return Ok(WalletConnectRequestApprovalOutcome::expired(
            relay_error.is_none(),
            relay_error,
            submitted_tx_hash,
        ));
    }
    if let Err(error) = &result
        && is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(error)
    {
        return Ok(
            WalletConnectRequestApprovalOutcome::hash_fallback_confirmation_required(
                walletconnect_hardware_typed_data_hash_fallback_confirmation_session(error),
            ),
        );
    }
    let authorization_failed = result
        .as_ref()
        .err()
        .is_some_and(is_walletconnect_authorization_error);
    let request_error = result.as_ref().err().map(format_report_chain);
    let response = match result {
        Ok(value) => WalletConnectJsonRpcResponse {
            id: response_request.item.id,
            jsonrpc: "2.0".to_owned(),
            result: Some(value),
            error: None,
        },
        Err(error) => build_walletconnect_jsonrpc_error(
            response_request.item.id,
            walletconnect_request_approval_error_kind(&response_request, &error),
            format_report_chain(&error),
        ),
    };
    let topic = response_request.session.session_topic.clone();
    let sym_key = response_request.session.keys.sym_key;
    if let Err(error) =
        publish_walletconnect_session_response(context.worker, topic, sym_key, response).await
    {
        if submitted_tx_hash.is_some() {
            return Ok(WalletConnectRequestApprovalOutcome {
                authorization_failed,
                response_published: false,
                submitted_tx_hash,
                relay_error: Some(error),
                request_error,
                expired: false,
                hash_fallback_confirmation_required: false,
                refreshed_hardware_session: None,
            });
        }
        return Err(error);
    }
    Ok(WalletConnectRequestApprovalOutcome {
        authorization_failed,
        response_published: true,
        submitted_tx_hash,
        relay_error: None,
        request_error,
        expired: false,
        hash_fallback_confirmation_required: false,
        refreshed_hardware_session: None,
    })
}

pub(super) async fn publish_walletconnect_expired_request_response(
    context: &WalletConnectClientContext,
    request: &WalletConnectRequestUi,
) -> Result<(), String> {
    let response = build_walletconnect_jsonrpc_error(
        request.item.id,
        WalletConnectRequestErrorKind::ExpiredRequest,
        "WalletConnect request expired before approval completed",
    );
    let topic = request.session.session_topic.clone();
    let sym_key = request.session.keys.sym_key;
    publish_walletconnect_session_response_ref(
        &context.worker,
        topic,
        &sym_key,
        response,
        WALLETCONNECT_RELAY_TTL_SECS,
        WC_SESSION_REQUEST_RESPONSE_TAG,
    )
    .await
}

pub(super) fn transaction_request_from_walletconnect(
    chain_id: u64,
    transaction: WalletConnectEvmTransaction,
) -> eyre::Result<TransactionRequest> {
    let mut tx = TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(transaction.from);
    if let Some(to) = transaction.to {
        tx = tx.with_to(to);
    }
    if let Some(value) = transaction.value {
        tx = tx.with_value(value);
    }
    if let Some(data) = transaction.data {
        let data = data.strip_prefix("0x").unwrap_or(&data);
        let bytes = alloy::hex::decode(data).map_err(|error| {
            eyre::eyre!("WalletConnect transaction data is invalid hex: {error}")
        })?;
        tx = tx.with_input(bytes);
    }
    if let Some(access_list) = transaction.access_list {
        tx = tx.access_list(access_list);
    }
    if let Some(gas) = transaction.gas {
        tx = tx.with_gas_limit(walletconnect_u256_to_u64(gas, "gas")?);
    }
    if let Some(gas_price) = transaction.gas_price {
        tx = tx.with_gas_price(walletconnect_u256_to_u128(gas_price, "gasPrice")?);
    }
    if let Some(max_fee_per_gas) = transaction.max_fee_per_gas {
        tx = tx.with_max_fee_per_gas(walletconnect_u256_to_u128(max_fee_per_gas, "maxFeePerGas")?);
    }
    if let Some(max_priority_fee_per_gas) = transaction.max_priority_fee_per_gas {
        tx = tx.with_max_priority_fee_per_gas(walletconnect_u256_to_u128(
            max_priority_fee_per_gas,
            "maxPriorityFeePerGas",
        )?);
    }
    if let Some(nonce) = transaction.nonce {
        tx = tx.with_nonce(walletconnect_u256_to_u64(nonce, "nonce")?);
    }
    if let Some(transaction_type) = transaction.transaction_type {
        tx = tx.transaction_type(transaction_type);
    }
    Ok(tx)
}

pub(super) fn walletconnect_u256_to_u64(value: U256, field: &str) -> eyre::Result<u64> {
    if value > U256::from(u64::MAX) {
        return Err(eyre::eyre!("WalletConnect transaction {field} exceeds u64"));
    }
    Ok(value.to::<u64>())
}

pub(super) fn walletconnect_u256_to_u128(value: U256, field: &str) -> eyre::Result<u128> {
    if value > U256::from(u128::MAX) {
        return Err(eyre::eyre!(
            "WalletConnect transaction {field} exceeds u128"
        ));
    }
    Ok(value.to::<u128>())
}

pub(super) fn walletconnect_personal_message_bytes(message: &str) -> Vec<u8> {
    if let Some(hex) = message.strip_prefix("0x")
        && hex.len().is_multiple_of(2)
        && let Ok(bytes) = alloy::hex::decode(hex)
    {
        return bytes;
    }
    message.as_bytes().to_vec()
}

pub(super) fn walletconnect_approval_should_publish_expired_response(
    expiry_timestamp: Option<u64>,
    now: u64,
    submitted_tx_hash: Option<&str>,
) -> bool {
    submitted_tx_hash.is_none() && walletconnect_pending_request_expired(expiry_timestamp, now)
}

pub(super) const fn walletconnect_request_approval_uses_expiry_timeout(
    parsed: &WalletConnectParsedRequest,
) -> bool {
    !matches!(
        parsed,
        WalletConnectParsedRequest::EthSendTransaction { .. }
    )
}

pub(super) fn is_walletconnect_authorization_error(error: &eyre::Report) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("password") || message.contains("authorize") || message.contains("spend")
}

pub(super) fn is_walletconnect_user_rejected_error(error: &eyre::Report) -> bool {
    let message = format_report_chain(error).to_ascii_lowercase();
    message.contains("cancelled")
        || message.contains("canceled")
        || message.contains("actioncancelled")
        || message.contains("user rejected")
        || message.contains("rejected on device")
        || message.contains("rejected on your ledger")
        || message.contains("request was rejected")
}

pub(super) fn walletconnect_request_approval_error_kind(
    request: &WalletConnectRequestUi,
    error: &eyre::Report,
) -> WalletConnectRequestErrorKind {
    if is_walletconnect_user_rejected_error(error) {
        WalletConnectRequestErrorKind::UserRejected
    } else if is_walletconnect_authorization_error(error) {
        WalletConnectRequestErrorKind::Unauthorized
    } else if request.account_source == PublicAccountSource::HardwareDerived
        && matches!(
            request.item.method,
            WalletConnectSupportedMethod::EthSignTypedData
                | WalletConnectSupportedMethod::EthSignTypedDataV4
        )
    {
        WalletConnectRequestErrorKind::UnsupportedMethod
    } else {
        WalletConnectRequestErrorKind::Internal
    }
}

pub(super) fn walletconnect_request_key(topic: &str, request_id: u64) -> String {
    format!("{topic}:{request_id}")
}

pub(super) fn first_walletconnect_pending_request_key(
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
) -> Option<String> {
    pending_requests.keys().next().cloned()
}

pub(super) fn next_walletconnect_auto_open_request_key(
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
    dismissed_request_dialog_keys: &BTreeSet<String>,
) -> Option<String> {
    pending_requests
        .keys()
        .find(|key| !dismissed_request_dialog_keys.contains(key.as_str()))
        .cloned()
}

pub(super) fn walletconnect_request_dialog_nav(
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
    request_key: &str,
) -> Option<WalletConnectRequestDialogNav> {
    let keys = pending_requests.keys().collect::<Vec<_>>();
    let position = keys.iter().position(|key| key.as_str() == request_key)?;
    Some(WalletConnectRequestDialogNav {
        index: position + 1,
        total: keys.len(),
        previous_key: position
            .checked_sub(1)
            .and_then(|index| keys.get(index))
            .map(|key| (*key).clone()),
        next_key: keys.get(position + 1).map(|key| (*key).clone()),
    })
}

pub(super) const fn walletconnect_request_matches_review_token(
    request: &WalletConnectRequestUi,
    review_token: u64,
) -> bool {
    request.review_token == review_token
}

pub(super) fn expired_walletconnect_request_keys(
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
    request_actions: &BTreeSet<String>,
    now: u64,
) -> Vec<String> {
    pending_requests
        .iter()
        .filter(|(key, request)| {
            !request_actions.contains(key.as_str())
                && request
                    .item
                    .expiry_timestamp
                    .is_some_and(|expiry| expiry <= now)
        })
        .map(|(key, _)| key.clone())
        .collect()
}

pub(super) fn remember_walletconnect_handled_request_key(
    handled_request_keys: &mut BTreeSet<String>,
    handled_request_key_order: &mut VecDeque<String>,
    request_key: String,
    max_keys: usize,
) {
    if max_keys == 0 || !handled_request_keys.insert(request_key.clone()) {
        return;
    }
    handled_request_key_order.push_back(request_key);
    while handled_request_keys.len() > max_keys {
        let Some(stale_key) = handled_request_key_order.pop_front() else {
            return;
        };
        handled_request_keys.remove(&stale_key);
    }
}

pub(super) fn walletconnect_request_should_queue(
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
    handled_request_keys: &BTreeSet<String>,
    request_key: &str,
) -> bool {
    !pending_requests.contains_key(request_key) && !handled_request_keys.contains(request_key)
}

#[cfg(test)]
pub(super) fn walletconnect_request_authorization_summary(
    request: &WalletConnectRequestUi,
    intent: &WalletConnectIntentView<'_>,
) -> SpendAuthorizationSummary {
    walletconnect_request_authorization_summary_with_fee(request, intent, None)
}

pub(super) fn walletconnect_request_authorization_summary_with_fee(
    request: &WalletConnectRequestUi,
    intent: &WalletConnectIntentView<'_>,
    reviewed_fee: Option<&WalletConnectReviewedFeeProjection>,
) -> SpendAuthorizationSummary {
    let requester = intent.provenance.dapp_name.as_ref().map_or_else(
        || intent.provenance.site.clone(),
        |name| format!("{} ({name})", intent.provenance.site),
    );
    let mut rows = vec![SpendAuthorizationSummaryRow::new("Requested by", requester)];
    rows.push(SpendAuthorizationSummaryRow::new(
        "Method",
        request.item.method.as_str().to_owned(),
    ));
    rows.push(SpendAuthorizationSummaryRow::new(
        "Chain",
        chain_label_for_caip2(&request.item.chain_id),
    ));
    if walletconnect_selected_account_provenance_visible(request.item.account, &intent.parties) {
        rows.push(
            SpendAuthorizationSummaryRow::new("Account", request.item.account.to_string())
                .with_shortened_copyable(),
        );
    }
    rows.push(
        SpendAuthorizationSummaryRow::new("Intent", intent.authorization.clone())
            .with_icon(intent.icon.clone()),
    );
    if let WalletConnectHeroSummary::TypedData(summary) = &intent.hero.summary {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Typed data",
            format!(
                "{} / {}",
                summary
                    .domain_name
                    .as_deref()
                    .unwrap_or("No domain name supplied"),
                summary.primary_type
            ),
        ));
    }
    for party in intent.parties.iter().filter(|party| {
        matches!(
            party.role,
            WalletConnectPartyRole::Contract
                | WalletConnectPartyRole::Recipient
                | WalletConnectPartyRole::Spender
        )
    }) {
        rows.push(
            SpendAuthorizationSummaryRow::new(
                walletconnect_party_role_label(party.role),
                party.address.to_checksum(None),
            )
            .with_shortened_copyable(),
        );
    }
    if let (Some(chain_id), Some(maximum)) = (
        parse_caip2_chain_id(&request.item.chain_id),
        reviewed_fee.and_then(|reviewed_fee| reviewed_fee.maximum_gas_cost),
    ) {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Maximum network cost",
            format_native_token_amount_for_display(chain_id, maximum),
        ));
    }
    let mut warnings: Vec<Arc<str>> = intent
        .risks
        .iter()
        .map(|risk| Arc::from(risk.authorization_label()))
        .collect();
    if let Some(attached_native) = intent.attached_native.as_ref()
        && !intent
            .risks
            .iter()
            .any(|risk| matches!(risk, WalletConnectRisk::AttachedNativeValue(_)))
        && !matches!(
            intent.action,
            WalletConnectIntentAction::NativeTransfer | WalletConnectIntentAction::Wrap
        )
    {
        warnings.push(Arc::from(
            WalletConnectRisk::AttachedNativeValue(attached_native.clone()).authorization_label(),
        ));
    }
    SpendAuthorizationSummary::new(
        "Authorize WalletConnect request",
        "Enter your vault password to authorize this request.",
        rows,
    )
    .with_warnings(warnings)
    .requiring_explicit_review()
}

pub(super) const fn hardware_walletconnect_notice(
    method: WalletConnectSupportedMethod,
) -> &'static str {
    match method {
        WalletConnectSupportedMethod::EthSignTypedData
        | WalletConnectSupportedMethod::EthSignTypedDataV4 => {
            "Confirm this EIP-712 typed-data request on the connected hardware wallet."
        }
        WalletConnectSupportedMethod::PersonalSign
        | WalletConnectSupportedMethod::EthSendTransaction => {
            "Confirm this WalletConnect request on the connected hardware wallet."
        }
        WalletConnectSupportedMethod::EthAccounts
        | WalletConnectSupportedMethod::EthRequestAccounts
        | WalletConnectSupportedMethod::WalletSwitchEthereumChain => {
            "This request does not require hardware confirmation."
        }
    }
}
