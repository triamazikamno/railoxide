//! Broker for a finite set of Ethereum reads through the shared wallet HTTP route.
//!
//! `submit` preserves method results. Calls and balances may reuse results or aggregate.
//! Other methods execute individually. Every decoded successful HTTP body is capped at 16 MiB.
//! Exact transaction-hash reads require a typed dapp origin; upstream consumers must authenticate
//! and authorize requests and resolve wallet-tracked hashes without exact-hash fallback.
//!
//! Execution has two concurrency levels: `max_in_flight` bounds top-level actor jobs, while the
//! shared semaphore separately bounds concurrent physical HTTP attempts created by reduction
//! fan-out inside a job.

mod actor;
mod broker;
mod cache;
mod execution;
mod model;
mod operation;
mod profile;
mod resolution;
mod scheduler;
#[cfg(test)]
pub(crate) mod tests;
pub use broker::RpcBroker;
pub(crate) use model::RpcBrokerViewCalls;
pub(crate) use model::{BalanceRead, TransactionHashRead, total_failure};
pub use model::{
    FailureClass, RpcBrokerError, RpcBrokerSpawnError, RpcChainRoute, RpcOrigin, RpcOriginError,
    RpcRead, RpcReadValidationError, RpcRemoteError, RpcResult, RpcRevert, RpcRoute, RpcSubmission,
    WalletRpcOrigin,
};
