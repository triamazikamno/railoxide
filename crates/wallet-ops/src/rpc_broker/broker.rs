use super::actor::{Actor, BlockEvent, Command, EndpointHealthOutcome, JobExecutor, RequestEvent};
use super::execution::run_job;
use super::identity::{CandidateOrder, random_candidate_order};
use super::model::{
    DEFAULT_INTERVAL, DEFAULT_MAX_IN_FLIGHT, RpcBrokerError, RpcBrokerSpawnError, RpcChainRoute,
    RpcClients, RpcOrigin, RpcRead, RpcResult, RpcRoute, RpcSubmission,
};
use super::warmup::{InstalledWarmup, RpcIdentityWarmup, WarmupRequest, same_installation};
use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;
use poi::SensitiveUrl;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::{self, Instant};

const SUBMISSION_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(256).expect("submission capacity must be positive");
/// Bounds a candidate lookup's local communication with the actor, independent of HTTP and
/// identity probe timing.
const METADATA_LOOKUP_LIMIT: Duration = Duration::from_secs(10);

/// Handle for the shared Ethereum read broker.
#[derive(Clone)]
pub struct RpcBroker {
    tx: mpsc::Sender<Command>,
    block_tx: mpsc::UnboundedSender<BlockEvent>,
    admission: Arc<Semaphore>,
    warmups: mpsc::UnboundedSender<WarmupRequest>,
    warmup: Arc<Mutex<InstalledWarmup>>,
}

impl fmt::Debug for RpcBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcBroker").finish_non_exhaustive()
    }
}

impl RpcBroker {
    /// Starts the broker on the current Tokio runtime. Wallet endpoints use `client` and direct
    /// endpoints `direct_client`.
    pub fn spawn(
        client: reqwest::Client,
        direct_client: reqwest::Client,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(RpcBrokerSpawnError::NoRuntime)?;
        Self::spawn_primitive(
            RpcClients::new(client, direct_client),
            &handle,
            DEFAULT_INTERVAL,
            DEFAULT_MAX_IN_FLIGHT,
            SUBMISSION_CAPACITY,
            default_executor(),
            random_candidate_order,
        )
    }

    /// Starts the broker on an explicit Tokio runtime handle, with clients as for `spawn`.
    pub fn spawn_on(
        client: reqwest::Client,
        direct_client: reqwest::Client,
        handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        Self::spawn_primitive(
            RpcClients::new(client, direct_client),
            handle,
            DEFAULT_INTERVAL,
            DEFAULT_MAX_IN_FLIGHT,
            SUBMISSION_CAPACITY,
            default_executor(),
            random_candidate_order,
        )
    }

    fn spawn_primitive(
        clients: RpcClients,
        handle: &tokio::runtime::Handle,
        interval: Duration,
        max_in_flight: usize,
        submission_capacity: NonZeroUsize,
        executor: JobExecutor,
        candidate_order: CandidateOrder,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        if submission_capacity.get() > Semaphore::MAX_PERMITS {
            return Err(RpcBrokerSpawnError::InvalidSubmissionCapacity);
        }
        let (tx, rx) = mpsc::channel(submission_capacity.get());
        let (block_tx, block_rx) = mpsc::unbounded_channel();
        let actor = Actor::new(
            clients,
            rx,
            interval,
            max_in_flight.max(1),
            executor,
            candidate_order,
        );
        let broker = Arc::new(Self {
            tx,
            block_tx,
            admission: Arc::new(Semaphore::new(submission_capacity.get())),
            warmups: actor.warmup_requests(),
            warmup: Arc::default(),
        });
        let actor = handle.spawn(actor.run(block_rx));
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
    Arc::new(|resources, group, endpoints, control| run_job(resources, group, endpoints, control))
}

impl RpcBroker {
    pub(crate) async fn submit_eth_calls(
        &self,
        route: RpcRoute,
        calls: Vec<(Address, Bytes)>,
        origin: RpcOrigin,
    ) -> Result<Vec<Result<Bytes, RpcBrokerError>>, RpcBrokerError> {
        self.submit_eth_calls_at(route, calls, BlockId::latest(), origin)
            .await
    }

    /// Like [`Self::submit_eth_calls`], with every call executed at `block`.
    pub(crate) async fn submit_eth_calls_at(
        &self,
        route: RpcRoute,
        calls: Vec<(Address, Bytes)>,
        block: BlockId,
        origin: RpcOrigin,
    ) -> Result<Vec<Result<Bytes, RpcBrokerError>>, RpcBrokerError> {
        let reads = calls
            .into_iter()
            .map(|(target, calldata)| RpcRead::eth_call_at(target, calldata, block))
            .collect();
        self.submit(RpcSubmission::new(route, reads, origin))
            .await
            .map(|results| {
                results
                    .into_iter()
                    .map(|result| {
                        result.and_then(|value| {
                            super::execution::decode_hex_value(value.expose_value())
                        })
                    })
                    .collect()
            })
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
        self.submit_calls_decoded_at::<C>(route, calls, BlockId::latest(), origin)
            .await
    }

    /// Like [`Self::submit_calls_decoded_as`], with every call executed at `block`.
    pub(crate) async fn submit_calls_decoded_at<C: SolCall + 'static>(
        &self,
        route: RpcRoute,
        calls: Vec<(Address, Bytes)>,
        block: BlockId,
        origin: RpcOrigin,
    ) -> Result<Vec<Result<C::Return, RpcBrokerError>>, RpcBrokerError> {
        self.submit_eth_calls_at(route, calls, block, origin)
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

    /// Submits reads in input order, preserving each method's JSON result and member errors.
    /// Missing delivery or a total deadline is an outer error. Successful HTTP bodies are capped
    /// at 16 MiB; oversized responses fail without failover or a health penalty.
    /// Exact transaction-hash reads require an authenticated, authorized upstream dapp consumer.
    /// That consumer must resolve wallet-tracked hashes without forwarding them here.
    pub async fn submit(
        &self,
        submission: RpcSubmission,
    ) -> Result<Vec<Result<RpcResult, RpcBrokerError>>, RpcBrokerError> {
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
    /// Returns `route`'s health-eligible endpoints in randomized order for a caller that sends its
    /// own requests; identity admission remains the caller's. The lookup takes no submission
    /// credit, so held submissions and running reads do not delay it. Enqueueing and the reply
    /// share one deadline: `deadline` or the metadata lookup limit, whichever is earlier.
    pub(crate) async fn endpoint_candidates(
        &self,
        route: &RpcChainRoute,
        deadline: Option<Instant>,
    ) -> Result<Vec<SensitiveUrl>, RpcBrokerError> {
        let limit = Instant::now() + METADATA_LOOKUP_LIMIT;
        let deadline = deadline.map_or(limit, |deadline| deadline.min(limit));
        let (reply, receiver) = oneshot::channel();
        let lookup = async {
            self.tx
                .send(Command::Candidates {
                    route: route.clone(),
                    reply,
                })
                .await
                .map_err(|_| RpcBrokerError::Shutdown)?;
            receiver.await.map_err(|_| RpcBrokerError::Shutdown)?
        };
        time::timeout_at(deadline, lookup)
            .await
            .map_err(|_| RpcBrokerError::Timeout)?
    }
    /// Reports the outcome of a request the caller sent to `endpoint` itself, without waiting.
    /// A strike withdraws the endpoint from every route's candidates at once; a healthy outcome
    /// ends its withdrawal. Reports go on the unbounded advisory channel, so they never wait on
    /// submission credit or a full command mailbox; a report sent after shutdown is dropped.
    pub(crate) fn report_endpoint_outcome(
        &self,
        chain_id: u64,
        endpoint: &url::Url,
        outcome: EndpointHealthOutcome,
    ) {
        let _ = self
            .block_tx
            .send(BlockEvent::EndpointReported(RequestEvent::new(
                chain_id,
                endpoint.clone(),
                outcome,
            )));
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
    /// Starts background identity warmup for an installation's routes and returns its owner.
    ///
    /// Pass clones of the installed route objects: checks land in their shared caches and are
    /// usable at once, while rebuilt routes have caches of their own. Initially each endpoint
    /// that needs verification is checked, and each endpoint of a trusted route gets an
    /// `eth_chainId` responsiveness probe that leaves it trusted. Failed endpoints stay out of
    /// candidate snapshots and are retried after the broker's usual cooldown until recovery.
    /// The pass uses at most four permits from the broker's independent verification budget.
    /// The broker runs one pass: a new installation replaces the previous pass, and registering the same
    /// initial route objects while their pass is owned joins it, including after more routes
    /// are activated. The owner can activate additional routes without restarting the pass.
    /// Dropping every handle cancels the pass. Pending checks for `preferred_chain` run first,
    /// including when joining a pass. Its end does not imply that every endpoint verified.
    #[must_use]
    pub fn warm_identities(
        &self,
        routes: Vec<RpcChainRoute>,
        preferred_chain: Option<u64>,
    ) -> RpcIdentityWarmup {
        let mut installed = self.warmup.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((current, owner)) = &*installed
            && same_installation(current, &routes)
            && let Some(owner) = owner.upgrade()
        {
            let warmup = RpcIdentityWarmup(owner);
            if let Some(chain_id) = preferred_chain {
                warmup.prioritize_chain(chain_id);
            }
            return warmup;
        }
        let (warmup, request) = RpcIdentityWarmup::start(routes.clone(), preferred_chain);
        *installed = Some((routes, Arc::downgrade(&warmup.0)));
        // Sent under the lock so the actor sees installations in registration order. Without an
        // actor the handle is inert.
        let _ = self.warmups.send(request);
        warmup
    }
}

#[cfg(test)]
#[path = "tests/broker.rs"]
mod tests;
