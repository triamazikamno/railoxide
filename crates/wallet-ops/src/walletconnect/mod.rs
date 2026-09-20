mod crypto;
mod eip155;
mod lifecycle;
mod namespace;
mod pairing;
mod relay;
mod request;
mod session;
mod uri;

pub use crypto::{
    WalletConnectEnvelope, decode_walletconnect_message, derive_walletconnect_session_sym_key,
    derive_walletconnect_session_topic, encode_walletconnect_message,
    generate_walletconnect_key_pair, hash_walletconnect_key,
};
pub use eip155::{
    WALLETCONNECT_EIP155_NAMESPACE, WalletConnectSessionRequest, WalletConnectSupportedEvent,
    WalletConnectSupportedMethod, WalletConnectTransactionRequest,
};
pub use lifecycle::{
    WalletConnectDisconnectPlan, WalletConnectRelayLifecycle, WalletConnectTerminalLifecycleEnd,
    build_walletconnect_disconnect_plan,
};
pub use namespace::{
    WalletConnectNamespaceAccountSupport, WalletConnectNamespaceNegotiation,
    WalletConnectNamespaceProposal, WalletConnectUnsupportedNamespaceItem,
    negotiate_walletconnect_namespaces, negotiate_walletconnect_namespaces_with_account_support,
};
pub use pairing::{
    WalletConnectPairingStart, WalletConnectProposalRejectionReason, WalletConnectProposalSummary,
    WalletConnectSessionApproval, WalletConnectSessionProposal, approve_walletconnect_session,
    approve_walletconnect_session_with_account_support, decode_walletconnect_session_proposal,
    reject_walletconnect_session_proposal, start_walletconnect_pairing,
};
pub use relay::{
    WALLETCONNECT_DEFAULT_PROJECT_ID, WALLETCONNECT_RELAY_RPC_URL, WALLETCONNECT_RELAY_URL,
    WalletConnectJsonRpcId, WalletConnectJsonRpcRequest, WalletConnectJsonRpcResponse,
    WalletConnectRelayClient, WalletConnectRelayClientAuth, WalletConnectRelayConfig,
    WalletConnectRelayJsonRpcResponse, WalletConnectRelayRpc, WalletConnectRelaySocket,
    WalletConnectRelaySubscriptionPayload, WalletConnectRelaySubscriptionRequest,
};
pub use request::{
    DappRequestValidationError, WalletConnectDecodedCallKind, WalletConnectDecodedTransaction,
    WalletConnectEvmTransaction, WalletConnectLifecycleRequestOutcome, WalletConnectParsedRequest,
    WalletConnectPendingRequest, WalletConnectPendingRequestQueue, WalletConnectRequestErrorKind,
    WalletConnectRequestValidation, build_walletconnect_jsonrpc_error,
    build_walletconnect_session_event, handle_walletconnect_lifecycle_request,
    parse_dapp_request_for_account, parse_walletconnect_session_request,
    validate_dapp_request_account, validate_walletconnect_session_request,
    validate_walletconnect_session_request_with_account_support,
};
pub use session::{
    WC_SESSION_PROPOSE, WC_SESSION_PROPOSE_RESPONSE_TAG, WC_SESSION_SETTLE,
    WC_SESSION_SETTLE_REQUEST_TAG, WalletConnectApprovalMessages, WalletConnectRelayStep,
};
pub use uri::{
    WALLETCONNECT_IRN_RELAY_PROTOCOL, WALLETCONNECT_REQUIRED_PAIRING_METHOD,
    WalletConnectPairingUri,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalletConnectError {
    #[error("invalid WalletConnect URI: {0}")]
    InvalidUri(String),
    #[error("WalletConnect URI has expired")]
    ExpiredUri,
    #[error("unsupported WalletConnect method {0}")]
    UnsupportedMethod(String),
    #[error("unsupported WalletConnect event {0}")]
    UnsupportedEvent(String),
    #[error("unsupported WalletConnect namespace {0}")]
    UnsupportedNamespace(String),
    #[error("unsupported WalletConnect chain {0}")]
    UnsupportedChain(String),
    #[error("unsatisfied WalletConnect namespaces: {0}")]
    UnsatisfiedNamespaces(String),
    #[error("malformed WalletConnect params: {0}")]
    MalformedParams(String),
    #[error("WalletConnect crypto failed")]
    Crypto,
    #[error("WalletConnect relay request failed: {0}")]
    Relay(String),
    #[error("encode WalletConnect JSON failed: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("WalletConnect HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

pub(crate) type Result<T> = std::result::Result<T, WalletConnectError>;

pub(crate) use request::WrappedNative;

#[cfg(test)]
mod tests;

pub(crate) use request::parse_dapp_chain_request;
