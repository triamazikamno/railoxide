//! Broker for a finite set of Ethereum reads through the shared wallet HTTP route.
//!
//! `submit` preserves method results. Calls and balances may reuse results or aggregate.
//! Other methods execute individually. Every decoded successful HTTP body is capped at 16 MiB.
//! Exact transaction-hash reads require a typed dapp origin; upstream consumers must authenticate
//! and authorize requests and resolve wallet-tracked hashes without exact-hash fallback.
//!
//! `max_in_flight` separately bounds jobs executing reads, jobs driving identity probes, and
//! concurrent physical HTTP attempts, including reduction fan-out inside a job. Identity probes
//! also share a smaller broker-wide budget so that verification alone cannot occupy every HTTP
//! permit.

mod actor;
mod broker;
mod cache;
mod candidates;
mod capacity;
mod execution;
mod identity;
mod model;
mod operation;
mod profile;
mod resolution;
mod scheduler;
#[cfg(test)]
pub(crate) mod tests;
mod warmup;
pub(crate) use actor::EndpointHealthOutcome;
pub use broker::RpcBroker;
pub(crate) use candidates::RpcCandidates;
pub(crate) use model::{BalanceRead, TransactionHashRead, total_failure};
pub use model::{
    ClassRoute, FailureClass, RpcBrokerError, RpcBrokerSpawnError, RpcChainRoute,
    RpcClassUnavailable, RpcEndpointRoles, RpcOrigin, RpcOriginError, RpcRead,
    RpcReadValidationError, RpcRemoteError, RpcResult, RpcRevert, RpcRoute, RpcSubmission,
    RpcTrafficClass, RpcTransport, WalletRpcOrigin,
};
pub(crate) use model::{RpcBrokerViewCalls, RpcClients};
pub use warmup::RpcIdentityWarmup;
