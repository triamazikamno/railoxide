//! Broker for submitted `eth_call` and `eth_getBalance` reads.
//!
//! Execution has two concurrency levels: `max_in_flight` bounds top-level actor jobs, while the
//! shared semaphore separately bounds concurrent physical HTTP attempts created by reduction
//! fan-out inside a job.

mod actor;
mod broker;
mod cache;
mod execution;
mod model;
mod profile;
mod resolution;
mod scheduler;
#[cfg(test)]
pub(crate) mod tests;
pub use broker::RpcBroker;
pub(crate) use model::total_failure;
pub use model::{
    FailureClass, RpcBrokerError, RpcBrokerSpawnError, RpcChainRoute, RpcOrigin, RpcOriginError,
    RpcRead, RpcReadValidationError, RpcRemoteError, RpcRevert, RpcRoute, RpcSubmission,
    WalletRpcOrigin,
};
