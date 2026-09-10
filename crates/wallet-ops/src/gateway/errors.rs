//! Provider error projection at the authorized response boundary.

use crate::rpc_broker::{RpcBrokerError, RpcReadValidationError, RpcRemoteError};
use alloy::primitives::hex;
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt;

/// Availability determined by the provider owner, never inferred from a broker fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderAvailability {
    Available,
    Disconnected,
    ChainUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalProviderFailure {
    UserRejected,
    Unauthorized,
    Unsupported,
    Disconnected,
    ChainUnavailable,
    InvalidParams,
    InvalidInput,
    NotFound,
    Unavailable,
    TransactionRejected,
    LimitExceeded,
    Internal,
}

/// Typed approval failure retained until the authorized provider response boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayApprovalFailure {
    Local(LocalProviderFailure),
    Broker(RpcBrokerError),
}

/// May contain remote secrets. Serialize or expose only for the validated response owner.
#[derive(Clone, Serialize)]
#[serde(transparent)]
pub struct ProviderRpcError(Value);

impl ProviderRpcError {
    #[must_use]
    pub fn local(failure: LocalProviderFailure) -> Self {
        let (code, message) = match failure {
            LocalProviderFailure::UserRejected => (4001, "User rejected the request"),
            LocalProviderFailure::Unauthorized => (4100, "Unauthorized"),
            LocalProviderFailure::Unsupported => (4200, "Unsupported method"),
            LocalProviderFailure::Disconnected => (4900, "Provider disconnected"),
            LocalProviderFailure::ChainUnavailable => (4901, "Chain unavailable"),
            LocalProviderFailure::InvalidParams => (-32602, "Invalid parameters"),
            LocalProviderFailure::InvalidInput => (-32000, "Invalid input"),
            LocalProviderFailure::NotFound => (-32001, "Resource not found"),
            LocalProviderFailure::Unavailable => (-32002, "Resource unavailable"),
            LocalProviderFailure::TransactionRejected => (-32003, "Transaction rejected"),
            LocalProviderFailure::LimitExceeded => (-32005, "Limit exceeded"),
            LocalProviderFailure::Internal => (-32603, "Internal error"),
        };
        Self(json!({"code": code, "message": message}))
    }

    #[must_use]
    pub fn from_broker(error: RpcBrokerError, availability: ProviderAvailability) -> Self {
        let failure = match error {
            RpcBrokerError::Remote(remote) => return Self::remote(&remote),
            RpcBrokerError::InnerRevert(revert) => {
                return revert.source().map_or_else(
                    || {
                        Self(json!({
                            "code": 3,
                            "message": "execution reverted",
                            "data": hex::encode_prefixed(revert.expose_bytes()),
                        }))
                    },
                    Self::remote,
                );
            }
            RpcBrokerError::OriginRejected => LocalProviderFailure::Unauthorized,
            RpcBrokerError::InvalidRead(RpcReadValidationError::InvalidParams) => {
                LocalProviderFailure::InvalidParams
            }
            RpcBrokerError::InvalidRead(RpcReadValidationError::ChainIdMismatch) => {
                LocalProviderFailure::InvalidInput
            }
            RpcBrokerError::ResponseTooLarge | RpcBrokerError::AdmissionRejected => {
                LocalProviderFailure::LimitExceeded
            }
            RpcBrokerError::Timeout | RpcBrokerError::TimeoutBeforeDispatch => {
                LocalProviderFailure::Unavailable
            }
            RpcBrokerError::Transport
            | RpcBrokerError::HttpStatus(_)
            | RpcBrokerError::NoEndpoint { .. }
            | RpcBrokerError::Shutdown => match availability {
                ProviderAvailability::Available => LocalProviderFailure::Unavailable,
                ProviderAvailability::Disconnected => LocalProviderFailure::Disconnected,
                ProviderAvailability::ChainUnavailable => LocalProviderFailure::ChainUnavailable,
            },
            RpcBrokerError::InvalidResponse => LocalProviderFailure::Internal,
        };
        Self::local(failure)
    }

    fn remote(remote: &RpcRemoteError) -> Self {
        serde_json::to_value(remote.expose_payload())
            .map_or_else(|_| Self::local(LocalProviderFailure::Internal), Self)
    }

    #[must_use]
    pub const fn expose_value(&self) -> &Value {
        &self.0
    }

    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl fmt::Debug for ProviderRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderRpcError { .. }")
    }
}

impl fmt::Display for ProviderRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider RPC error")
    }
}
