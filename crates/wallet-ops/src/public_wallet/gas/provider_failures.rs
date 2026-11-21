#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn classify_broker_provider_failure(
    error: &crate::rpc_broker::RpcBrokerError,
) -> ProviderFailure {
    use crate::rpc_broker::RpcBrokerError;

    match error {
        RpcBrokerError::InnerRevert(revert) => {
            let data = revert.expose_bytes().to_vec();
            ProviderFailure {
                reason: friendly_revert_data(&data),
                group: ProviderFailureGroup::RevertData(data),
            }
        }
        RpcBrokerError::Remote(remote) => ProviderFailure {
            group: ProviderFailureGroup::JsonRpc {
                code: remote.code(),
                message: String::new(),
            },
            reason: format!("RPC error {}.", remote.code()),
        },
        RpcBrokerError::HttpStatus(status) => ProviderFailure {
            group: ProviderFailureGroup::Http(*status),
            reason: format!("RPC returned HTTP status {status}."),
        },
        _ => ProviderFailure {
            group: ProviderFailureGroup::Other,
            reason: "RPC provider is unavailable.".to_owned(),
        },
    }
}

pub(super) fn classify_provider_failure(
    error: &alloy::transports::TransportError,
) -> ProviderFailure {
    if let Some(response) = error.as_error_resp() {
        if let Some(data) = response.as_revert_data() {
            let data = data.to_vec();
            return ProviderFailure {
                reason: friendly_revert_data(&data),
                group: ProviderFailureGroup::RevertData(data),
            };
        }
        if let Some(reason) = friendly_revert_message(&response.message) {
            return ProviderFailure {
                group: ProviderFailureGroup::RevertMessage(reason.to_ascii_lowercase()),
                reason,
            };
        }
        let message = safe_provider_message(&response.message);
        return ProviderFailure {
            group: ProviderFailureGroup::JsonRpc {
                code: response.code,
                message: message.to_ascii_lowercase(),
            },
            reason: if message.is_empty() {
                format!("RPC error {}.", response.code)
            } else {
                format!("RPC error {}: {message}", response.code)
            },
        };
    }

    if let Some(transport) = error.as_transport_err()
        && let Some(http) = transport.as_http_error()
    {
        return ProviderFailure {
            group: ProviderFailureGroup::Http(http.status),
            reason: format!("RPC returned HTTP status {}.", http.status),
        };
    }

    ProviderFailure {
        group: ProviderFailureGroup::Other,
        reason: "RPC provider is unavailable.".to_owned(),
    }
}

pub(super) fn select_provider_failure(
    failures: Vec<ProviderFailure>,
) -> PublicAdvancedTransactionSimulationError {
    const CONFLICTING_RESULTS: &str = "RPC providers returned conflicting simulation results.";

    let mut groups = HashMap::<ProviderFailureGroup, (usize, String)>::new();
    for failure in failures {
        let entry = groups.entry(failure.group).or_insert((0, failure.reason));
        entry.0 += 1;
    }
    let Some((group, (highest_count, reason))) = groups.iter().max_by_key(|(_, (count, _))| *count)
    else {
        return unavailable_simulation_error("RPC providers are unavailable.");
    };
    if groups
        .values()
        .filter(|(count, _)| count == highest_count)
        .count()
        != 1
    {
        return unavailable_simulation_error(CONFLICTING_RESULTS);
    }
    match group {
        ProviderFailureGroup::RevertData(_) | ProviderFailureGroup::RevertMessage(_) => {
            PublicAdvancedTransactionSimulationError::Reverted(reason.clone())
        }
        ProviderFailureGroup::JsonRpc { .. }
        | ProviderFailureGroup::Http(_)
        | ProviderFailureGroup::Other => {
            PublicAdvancedTransactionSimulationError::Unavailable(reason.clone())
        }
    }
}

pub(super) fn unavailable_simulation_error(
    reason: &str,
) -> PublicAdvancedTransactionSimulationError {
    PublicAdvancedTransactionSimulationError::Unavailable(reason.to_owned())
}

pub(super) fn simulation_error_reason(error: &PublicAdvancedTransactionSimulationError) -> &str {
    match error {
        PublicAdvancedTransactionSimulationError::RpcRead(_) => "RPC read is unavailable.",
        PublicAdvancedTransactionSimulationError::Reverted(reason)
        | PublicAdvancedTransactionSimulationError::Unavailable(reason) => reason,
    }
}

pub(super) fn friendly_revert_data(data: &[u8]) -> String {
    let Some(selector) = data.get(..4) else {
        return "Execution reverted".to_owned();
    };
    let standard_selector = selector == Revert::SELECTOR || selector == Panic::SELECTOR;
    if standard_selector && let Some(reason) = decode_revert_reason(data) {
        let reason = safe_provider_message(reason.strip_prefix("revert: ").unwrap_or(&reason));
        return if reason.is_empty() {
            "Execution reverted".to_owned()
        } else {
            reason
        };
    }
    format!("Custom error 0x{}", alloy::hex::encode(selector))
}

fn friendly_revert_message(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let marker = ["execution reverted", "reverted"]
        .into_iter()
        .filter_map(|marker| lower.find(marker).map(|index| (index, marker)))
        .min_by_key(|(index, _)| *index)?;
    let suffix = message[marker.0 + marker.1.len()..]
        .trim_start_matches([' ', ':', '-'])
        .trim();
    Some(if suffix.is_empty() {
        "Execution reverted".to_owned()
    } else {
        let reason = safe_provider_message(suffix);
        if reason.is_empty() {
            "Execution reverted".to_owned()
        } else {
            reason
        }
    })
}

fn safe_provider_message(message: &str) -> String {
    const MAX_MESSAGE_CHARS: usize = 160;
    let mut message = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = message.chars().take(MAX_MESSAGE_CHARS - 3).collect();
        message.push_str("...");
    }
    message
}
