use super::actor::{Actor, BlockEvent, Command, JobExecutor};
use super::execution::run_job;
use super::model::{
    DEFAULT_INTERVAL, DEFAULT_MAX_IN_FLIGHT, RpcBrokerError, RpcBrokerSpawnError, RpcOrigin,
    RpcRead, RpcRoute, RpcSubmission,
};
use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::{self, Instant};

const SUBMISSION_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(256).expect("submission capacity must be positive");

/// Handle for submitted `eth_call` and `eth_getBalance` reads.
#[derive(Clone)]
pub struct RpcBroker {
    tx: mpsc::Sender<Command>,
    block_tx: mpsc::UnboundedSender<BlockEvent>,
    admission: Arc<Semaphore>,
}

impl fmt::Debug for RpcBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcBroker").finish_non_exhaustive()
    }
}

impl RpcBroker {
    /// Starts the broker on the current Tokio runtime.
    pub fn spawn(client: reqwest::Client) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(RpcBrokerSpawnError::NoRuntime)?;
        Self::spawn_primitive(
            client,
            &handle,
            DEFAULT_INTERVAL,
            DEFAULT_MAX_IN_FLIGHT,
            SUBMISSION_CAPACITY,
            default_executor(),
        )
    }

    /// Starts the broker on an explicit Tokio runtime handle.
    pub fn spawn_on(
        client: reqwest::Client,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        Self::spawn_primitive(
            client,
            handle,
            DEFAULT_INTERVAL,
            DEFAULT_MAX_IN_FLIGHT,
            SUBMISSION_CAPACITY,
            default_executor(),
        )
    }

    fn spawn_primitive(
        client: reqwest::Client,
        handle: &tokio::runtime::Handle,
        interval: Duration,
        max_in_flight: usize,
        submission_capacity: NonZeroUsize,
        executor: JobExecutor,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        if submission_capacity.get() > Semaphore::MAX_PERMITS {
            return Err(RpcBrokerSpawnError::InvalidSubmissionCapacity);
        }
        let (tx, rx) = mpsc::channel(submission_capacity.get());
        let (block_tx, block_rx) = mpsc::unbounded_channel();
        let broker = Arc::new(Self {
            tx,
            block_tx,
            admission: Arc::new(Semaphore::new(submission_capacity.get())),
        });
        let actor = handle
            .spawn(Actor::new(client, rx, interval, max_in_flight.max(1), executor).run(block_rx));
        if actor.is_finished() {
            return Err(RpcBrokerSpawnError::ActorStartup);
        }
        let _supervisor = handle.spawn(async move {
            if let Err(error) = actor.await {
                if error.is_panic() {
                    tracing::error!("RPC broker actor terminated by panic");
                } else if error.is_cancelled() {
                    tracing::error!("RPC broker actor task was cancelled");
                }
            }
        });
        Ok(broker)
    }
}

pub(super) fn default_executor() -> JobExecutor {
    Arc::new(|client, semaphore, group, endpoints| run_job(client, semaphore, group, endpoints))
}

impl RpcBroker {
    pub(crate) async fn submit_eth_calls(
        &self,
        route: RpcRoute,
        calls: Vec<(Address, Bytes)>,
        origin: RpcOrigin,
    ) -> Result<Vec<Result<Bytes, RpcBrokerError>>, RpcBrokerError> {
        let reads = calls
            .into_iter()
            .map(|(target, calldata)| RpcRead::eth_call(target, calldata))
            .collect();
        self.submit(RpcSubmission::new(route, reads, origin)).await
    }

    /// Submits pre-encoded calls in input order, with `C` selecting the response decoder; each
    /// member independently returns a typed value or broker error. The outer `Ok` always contains
    /// exactly one result per input call; missing delivery or a total deadline is an outer error.
    pub(crate) async fn submit_calls_decoded_as<C: SolCall + 'static>(
        &self,
        route: RpcRoute,
        calls: Vec<(Address, Bytes)>,
        origin: RpcOrigin,
    ) -> Result<Vec<Result<C::Return, RpcBrokerError>>, RpcBrokerError> {
        self.submit_eth_calls(route, calls, origin)
            .await
            .map(|results| {
                results
                    .into_iter()
                    .map(|result| {
                        result.and_then(|bytes| {
                            C::abi_decode_returns_validate(&bytes)
                                .map_err(|_| RpcBrokerError::InvalidResponse)
                        })
                    })
                    .collect()
            })
    }

    /// Submits reads in input order. The outer `Ok` always contains exactly one result per input read,
    /// including member errors; missing delivery or a total deadline is an outer error.
    pub async fn submit(
        &self,
        submission: RpcSubmission,
    ) -> Result<Vec<Result<Bytes, RpcBrokerError>>, RpcBrokerError> {
        submission.validate_admission()?;
        let read_count = submission.reads().len();
        if read_count == 0 {
            if self.tx.is_closed() {
                return Err(RpcBrokerError::Shutdown);
            }
            return Ok(Vec::new());
        }
        let deadline = submission
            .route()
            .request_timeout
            .map(|timeout| Instant::now() + timeout);
        let acquire = async {
            tokio::select! {
                () = self.tx.closed() => Err(RpcBrokerError::Shutdown),
                permit = self.admission.clone().acquire_owned() => permit
                    .map_err(|_| RpcBrokerError::Shutdown),
            }
        };
        let owned_permit = match deadline {
            Some(deadline) => time::timeout_at(deadline, acquire)
                .await
                .map_err(|_| RpcBrokerError::Timeout)??,
            None => acquire.await?,
        };
        let permit = Arc::new(owned_permit);
        let mut receivers = Vec::with_capacity(read_count);
        let mut replies = Vec::with_capacity(read_count);
        for _ in submission.reads() {
            let (reply, receiver) = oneshot::channel();
            replies.push(super::resolution::ReadReply::new(reply, permit.clone()));
            receivers.push(receiver);
        }
        drop(permit);
        match deadline {
            Some(deadline) => time::timeout_at(
                deadline,
                self.tx.send(Command::Submit {
                    submission: Box::new(submission),
                    replies,
                    deadline: Some(deadline),
                }),
            )
            .await
            .map_err(|_| RpcBrokerError::Timeout)?
            .map_err(|_| RpcBrokerError::Shutdown)?,
            None => self
                .tx
                .send(Command::Submit {
                    submission: Box::new(submission),
                    replies,
                    deadline,
                })
                .await
                .map_err(|_| RpcBrokerError::Shutdown)?,
        }
        let mut results = Vec::with_capacity(receivers.len());
        for receiver in receivers {
            let result = match deadline {
                Some(deadline) => time::timeout_at(deadline, receiver)
                    .await
                    .map_err(|_| RpcBrokerError::Timeout)?
                    .map_err(|_| RpcBrokerError::Shutdown)?,
                None => receiver.await.map_err(|_| RpcBrokerError::Shutdown)?,
            };
            results.push(result);
        }
        Ok(results)
    }
    /// Advises the broker of a head; newer heads advance state, equal heads refresh freshness,
    /// and stale heads are ignored.
    pub fn notify_block(&self, chain_id: u64, block_number: u64) {
        let _ = self.block_tx.send(BlockEvent::HeadObserved {
            chain_id,
            block_number,
        });
    }
    /// Disables latest-head usability and clears latest cache entries until a new head arrives.
    pub fn invalidate_block(&self, chain_id: u64) {
        let _ = self.block_tx.send(BlockEvent::Invalidate { chain_id });
    }
}

#[cfg(test)]
#[path = "tests/broker.rs"]
mod tests;
