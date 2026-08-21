use super::*;

pub(super) const WALLETCONNECT_TRANSACTION_DETAILS_LABEL: &str = "Transaction details";
pub(super) const WALLETCONNECT_RAW_REQUEST_LABEL: &str = "Raw request";
pub(super) const WALLETCONNECT_NETWORK_FEE_LABEL: &str = "Network fee";

pub(super) fn parse_caip2_chain_id(value: &str) -> Option<u64> {
    value.strip_prefix("eip155:")?.parse().ok()
}

pub(super) fn ethereum_chain_id_hex(chain_id: u64) -> String {
    format!("0x{chain_id:x}")
}

pub(super) fn walletconnect_transaction_selector(
    transaction: &WalletConnectEvmTransaction,
) -> Option<[u8; 4]> {
    let data = transaction.data.as_deref()?;
    let data = data.strip_prefix("0x").unwrap_or(data);
    let bytes = alloy::hex::decode(data).ok()?;
    bytes.get(..4)?.try_into().ok()
}

pub(super) fn walletconnect_enabled_chain_ids(
    effective_chain_configs: &BTreeMap<u64, EffectiveChainConfig>,
) -> BTreeSet<u64> {
    effective_chain_configs
        .iter()
        .filter_map(|(chain_id, config)| config.enabled.then_some(*chain_id))
        .collect()
}

pub(super) fn ensure_walletconnect_chain_id_enabled(
    chain_id: &str,
    enabled_chain_ids: &BTreeSet<u64>,
) -> Result<(), WalletConnectSessionRequestFailure> {
    let Some(chain_id_value) = parse_caip2_chain_id(chain_id) else {
        return Err(WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::UnsupportedChain,
            message: format!("Unsupported WalletConnect chain: {chain_id}"),
        });
    };
    if enabled_chain_ids.contains(&chain_id_value) {
        Ok(())
    } else {
        Err(WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::UnsupportedChain,
            message: format!("WalletConnect chain is disabled: {chain_id}"),
        })
    }
}

pub(super) fn walletconnect_namespace_account_support(
    account: &PublicAccountMetadata,
    view_session: Option<&DesktopViewSession>,
) -> WalletConnectNamespaceAccountSupport {
    if account.source != PublicAccountSource::HardwareDerived {
        return WalletConnectNamespaceAccountSupport::for_account_source(account.source);
    }
    let mode = account.hardware_descriptor.as_ref().and_then(|descriptor| {
        view_session
            .and_then(DesktopViewSession::hardware_profile_session)
            .and_then(|session| session.typed_data_signing_mode(descriptor))
    });
    match mode {
        Some(mode) => WalletConnectNamespaceAccountSupport::hardware(mode),
        None => WalletConnectNamespaceAccountSupport::hardware_typed_data_capability_unknown(),
    }
}

pub(super) fn walletconnect_hardware_typed_data_mode_for_request(
    request: &WalletConnectRequestUi,
    public_accounts: &[PublicAccountMetadata],
    view_session: Option<&DesktopViewSession>,
) -> HardwareTypedDataSigningMode {
    if request.account_source != PublicAccountSource::HardwareDerived {
        return HardwareTypedDataSigningMode::Unsupported;
    }
    public_accounts
        .iter()
        .find(|account| {
            account.public_account_uuid == request.session.selected_public_account_uuid
                && account.status == PublicAccountStatus::Active
                && account.scope == request.session.selected_public_account_scope
        })
        .map_or(HardwareTypedDataSigningMode::Unsupported, |account| {
            walletconnect_namespace_account_support(account, view_session)
                .hardware_typed_data_signing_mode
        })
}

pub(super) fn walletconnect_proposal_requests_required_typed_data(
    proposal: &WalletConnectSessionProposal,
) -> bool {
    proposal.required_namespaces.values().any(|namespace| {
        namespace.methods.iter().any(|method| {
            method == WalletConnectSupportedMethod::EthSignTypedData.as_str()
                || method == WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()
        })
    })
}

#[cfg(feature = "hardware")]
pub(super) fn walletconnect_proposal_requests_optional_typed_data(
    proposal: &WalletConnectSessionProposal,
) -> bool {
    proposal.optional_namespaces.values().any(|namespace| {
        namespace.methods.iter().any(|method| {
            method == WalletConnectSupportedMethod::EthSignTypedData.as_str()
                || method == WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()
        })
    })
}

#[cfg(feature = "hardware")]
pub(super) fn walletconnect_proposal_requests_hardware_typed_data(
    proposal: &WalletConnectSessionProposal,
) -> bool {
    walletconnect_proposal_requests_required_typed_data(proposal)
        || walletconnect_proposal_requests_optional_typed_data(proposal)
}

pub(super) const fn walletconnect_session_expired(
    session: &WalletConnectSessionRecord,
    now: u64,
) -> bool {
    session.expiry_timestamp <= now
}

pub(super) const fn walletconnect_session_visible_in_management(
    session: &WalletConnectSessionRecord,
) -> bool {
    matches!(
        session.lifecycle_state,
        WalletConnectSessionLifecycleState::Active
            | WalletConnectSessionLifecycleState::TemporarilyPaused
            | WalletConnectSessionLifecycleState::Invalid
            | WalletConnectSessionLifecycleState::Disconnected
            | WalletConnectSessionLifecycleState::Expired
    )
}

pub(super) const fn walletconnect_session_has_expiring_lifecycle(
    session: &WalletConnectSessionRecord,
) -> bool {
    matches!(
        session.lifecycle_state,
        WalletConnectSessionLifecycleState::Active
            | WalletConnectSessionLifecycleState::TemporarilyPaused
    )
}

pub(super) const fn walletconnect_session_relay_processable(
    session: &WalletConnectSessionRecord,
    now: u64,
) -> bool {
    walletconnect_session_has_expiring_lifecycle(session)
        && !walletconnect_session_expired(session, now)
}

pub(super) fn walletconnect_pairing_expired(pairing: &WalletConnectPairingUri, now: u64) -> bool {
    pairing.expiry_timestamp.is_some_and(|expiry| expiry <= now)
}

pub(super) fn expired_walletconnect_pairing_topics(
    pairings: &BTreeMap<String, WalletConnectPairingUri>,
    now: u64,
) -> Vec<String> {
    pairings
        .iter()
        .filter(|(_, pairing)| walletconnect_pairing_expired(pairing, now))
        .map(|(topic, _)| topic.clone())
        .collect()
}

pub(super) fn walletconnect_pending_request_expired(
    expiry_timestamp: Option<u64>,
    now: u64,
) -> bool {
    expiry_timestamp.is_some_and(|expiry| expiry <= now)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectRequestExpiryStatus {
    Missing,
    Remaining(u64),
    Expired,
}

pub(super) const fn walletconnect_request_expiry_status(
    expiry_timestamp: Option<u64>,
    now: u64,
) -> WalletConnectRequestExpiryStatus {
    let Some(expiry_timestamp) = expiry_timestamp else {
        return WalletConnectRequestExpiryStatus::Missing;
    };
    let remaining = expiry_timestamp.saturating_sub(now);
    if remaining == 0 {
        WalletConnectRequestExpiryStatus::Expired
    } else {
        WalletConnectRequestExpiryStatus::Remaining(remaining)
    }
}

pub(super) const fn walletconnect_request_approval_admitted(
    expiry_timestamp: Option<u64>,
    now: u64,
) -> bool {
    matches!(
        walletconnect_request_expiry_status(expiry_timestamp, now),
        WalletConnectRequestExpiryStatus::Missing | WalletConnectRequestExpiryStatus::Remaining(_)
    )
}

pub(super) fn walletconnect_validate_pending_request_expiry(
    expiry_timestamp: Option<u64>,
    now: u64,
) -> Result<(), WalletConnectSessionRequestFailure> {
    if walletconnect_pending_request_expired(expiry_timestamp, now) {
        return Err(WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::ExpiredRequest,
            message: "WalletConnect request expired before approval".to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) struct WalletConnectRequestExpired;

pub(super) async fn walletconnect_await_before_request_expiry<F, T>(
    expiry_timestamp: Option<u64>,
    future: F,
) -> Result<T, WalletConnectRequestExpired>
where
    F: Future<Output = T>,
{
    let Some(expiry_timestamp) = expiry_timestamp else {
        return Ok(future.await);
    };
    let Some(duration) = walletconnect_duration_until_expiry(expiry_timestamp) else {
        return Err(WalletConnectRequestExpired);
    };
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| WalletConnectRequestExpired)
}

pub(super) fn walletconnect_duration_until_expiry(expiry_timestamp: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let expiry = Duration::from_secs(expiry_timestamp);
    if expiry <= now {
        return None;
    }
    expiry.checked_sub(now)
}

pub(super) fn walletconnect_proposal_rejection_reason(
    proposal: &WalletConnectSessionProposal,
    selected_account: Option<&PublicAccountMetadata>,
    selected_account_support: Option<WalletConnectNamespaceAccountSupport>,
    supported_chain_ids: &BTreeSet<u64>,
    now: u64,
) -> WalletConnectProposalRejectionReason {
    if proposal.is_expired(now) {
        return WalletConnectProposalRejectionReason::Expired;
    }
    if let Some(account) = selected_account
        && negotiate_walletconnect_namespaces_with_account_support(
            &proposal.required_namespaces,
            &proposal.optional_namespaces,
            supported_chain_ids,
            account.address,
            selected_account_support.unwrap_or_else(|| {
                WalletConnectNamespaceAccountSupport::for_account_source(account.source)
            }),
        )
        .is_err()
    {
        return WalletConnectProposalRejectionReason::UnsupportedNamespaces;
    }
    WalletConnectProposalRejectionReason::UserRejected
}

pub(super) fn walletconnect_session_request_expiry_timestamp(
    request_params: &Value,
    request_payload: &Value,
) -> Result<Option<u64>, WalletConnectSessionRequestFailure> {
    for (container, field) in [
        (request_payload, "expiryTimestamp"),
        (request_payload, "expiry"),
        (request_params, "expiryTimestamp"),
        (request_params, "expiry"),
    ] {
        let Some(value) = container.get(field) else {
            continue;
        };
        let Some(expiry) = walletconnect_json_u64_value(value) else {
            return Err(WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::MalformedParams,
                message: format!("WalletConnect request {field} must be a timestamp"),
            });
        };
        return Ok(Some(expiry));
    }
    Ok(None)
}

pub(super) fn walletconnect_json_u64_value(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let text = value.as_str()?;
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

pub(super) fn walletconnect_topic_log_label(topic: &str) -> String {
    const PREFIX_CHARS: usize = 10;
    let prefix = topic.chars().take(PREFIX_CHARS).collect::<String>();
    if prefix.len() == topic.len() {
        prefix
    } else {
        format!("{prefix}.../{}", topic.len())
    }
}

pub(super) fn walletconnect_request_key_log_label(value: &str) -> String {
    const PREFIX_CHARS: usize = 12;
    let prefix = value.chars().take(PREFIX_CHARS).collect::<String>();
    if prefix.len() == value.len() {
        prefix
    } else {
        format!("{prefix}.../{}", value.len())
    }
}

pub(super) fn walletconnect_is_transient_relay_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("walletconnect relay error")
        || lower.contains("relay websocket")
        || lower.contains("relay response timed out")
        || lower.contains("websocket upgrade failed")
        || lower.contains("relay transport")
        || lower.contains("read relay")
        || lower.contains("send relay")
}

pub(super) fn walletconnect_relay_request_was_not_sent(error: &str) -> bool {
    error.contains("request was not sent")
}

pub(super) fn walletconnect_session_uuid(proposal: &WalletConnectSessionProposal) -> String {
    format!("wc-{}-{}", proposal.pairing_topic, proposal.id)
}

pub(super) fn walletconnect_request_id_seed() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let entropy = WALLETCONNECT_RELAY_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        % WALLETCONNECT_RELAY_ID_ENTROPY_FACTOR;
    millis
        .saturating_mul(WALLETCONNECT_RELAY_ID_ENTROPY_FACTOR)
        .saturating_add(entropy)
}

pub(super) fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn walletconnect_error_message(error: &WalletConnectError) -> String {
    match error {
        WalletConnectError::InvalidUri(message) => {
            format!("The pasted data is not a valid WalletConnect URI: {message}")
        }
        WalletConnectError::ExpiredUri => "The WalletConnect URI or request has expired".to_owned(),
        WalletConnectError::UnsupportedMethod(method) => {
            format!("Unsupported WalletConnect method: {method}")
        }
        WalletConnectError::UnsupportedEvent(event) => {
            format!("Unsupported WalletConnect event: {event}")
        }
        WalletConnectError::UnsupportedNamespace(namespace) => {
            format!("Unsupported WalletConnect namespace: {namespace}")
        }
        WalletConnectError::UnsupportedChain(chain) => {
            format!("Unsupported WalletConnect chain: {chain}")
        }
        WalletConnectError::UnsatisfiedNamespaces(message) => {
            format!("Required WalletConnect namespaces cannot be satisfied: {message}")
        }
        WalletConnectError::MalformedParams(message) => {
            format!("Malformed WalletConnect request params: {message}")
        }
        WalletConnectError::Crypto => "WalletConnect message crypto failed".to_owned(),
        WalletConnectError::Relay(message) => format!("WalletConnect relay error: {message}"),
        WalletConnectError::Encode(error) => format!("WalletConnect JSON encoding failed: {error}"),
        WalletConnectError::Http(error) => format!("WalletConnect HTTP request failed: {error}"),
    }
}
