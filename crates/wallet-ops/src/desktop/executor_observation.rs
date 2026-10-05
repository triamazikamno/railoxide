use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alloy::consensus::{Transaction as _, TxReceipt};
use alloy::network::TransactionResponse as _;
use alloy::network::primitives::{BlockTransactions, HeaderResponse as _};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::{DynProvider, EthGetBlock, Provider as _};
use alloy::rpc::types::{Log, TransactionReceipt};
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::{RpcError, TransportError};
use broadcaster_core::contracts::railgun::{
    Call, Nullified, RelayAdapt7702, Shield, ShieldRequest, Transact, Transaction, shieldCall,
};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool, RpcAdmission};
use eyre::{Result, eyre};
use tracing::Instrument as _;

use crate::HttpContext;
use crate::block_observer::fetch_checked_block_receipts;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    ExecutorExecutionResult, ExecutorNonceObservation, ExecutorOperationId,
    ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload,
    IssuedExecutorRecoveryTransaction,
};

const MAX_OBSERVATION_BLOCKS: u64 = 64;
/// How long an endpoint whose request failed sits out executor observations.
const ENDPOINT_COOLDOWN: Duration = Duration::from_mins(3);
/// How long in all one observation waits in [`ObservationEndpoints::later_providers`]: just
/// above the RPC identity check's timeout, by which every endpoint's first check finished.
/// Tests wait less.
pub(super) const LATE_ADMISSION_WAIT: Duration =
    Duration::from_secs(if cfg!(test) { 2 } else { 11 });
/// How often a wait for a later admission looks at the pool.
const LATE_ADMISSION_POLL: Duration = Duration::from_millis(100);

mod recovery;

/// Time observation work without recording account data or RPC error contents.
pub(super) fn trace_step<T, E>(
    step: &'static str,
    future: impl Future<Output = Result<T, E>>,
) -> impl Future<Output = Result<T, E>> {
    // Keep nested timing wrappers from multiplying observation futures on the stack.
    let future = Box::pin(future);
    async move {
        let started = Instant::now();
        tracing::debug!(target: "executor_observation", step, "started");
        let result = future.await;
        tracing::debug!(
            target: "executor_observation",
            step,
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "finished"
        );
        result
    }
}

/// The chain endpoints one executor owner observes through, kept for its session.
///
/// Admission is the wallet's RPC identity admission: a route that requires identity
/// checks verifies each endpoint's chain once, and a failed check is retried in the
/// background with backoff while the owner lives, not on every observation. An
/// endpoint whose request fails sits out [`ENDPOINT_COOLDOWN`] unless it is the last
/// one available. The endpoint that served the last observation is tried first.
///
/// The pool lives until [`Self::release`]. Admission's background retries hold only
/// a weak reference to it, so releasing the pool lets them stop. A released owner
/// never rebuilds the pool and has no endpoints.
///
/// It also remembers, for this session only, which inclusions a full canonical
/// read of their block confirmed, so that a later observation can recheck those
/// by header alone.
pub(super) struct ObservationEndpoints {
    route: crate::RpcChainRoute,
    client: reqwest::Client,
    /// Completes once the first admission returned, whether or not the pool was kept.
    admitted: tokio::sync::OnceCell<()>,
    /// Never locked across an await.
    pool: Mutex<PoolSlot>,
    /// Pool index of the last endpoint that served an observation, or `usize::MAX`.
    preferred: AtomicUsize,
    /// Per operation, the inclusions the last successful observation read or
    /// rechecked on the canonical chain.
    verified: Mutex<BTreeMap<ExecutorOperationId, Vec<(B256, ExecutorPayloadInclusion)>>>,
}

#[derive(Default)]
struct PoolSlot {
    pool: Option<Arc<QueryRpcPool>>,
    /// Terminal: once set, the pool is gone and is never published again.
    released: bool,
}

impl ObservationEndpoints {
    pub(super) fn new(chain: &EffectiveChainConfig, http: &HttpContext) -> Self {
        Self {
            route: chain.rpc_route.clone(),
            client: http.rpc_client.clone(),
            admitted: tokio::sync::OnceCell::new(),
            pool: Mutex::new(PoolSlot::default()),
            preferred: AtomicUsize::new(usize::MAX),
            verified: Mutex::new(BTreeMap::new()),
        }
    }

    fn verified_inclusions(
        &self,
        operation: ExecutorOperationId,
    ) -> Vec<(B256, ExecutorPayloadInclusion)> {
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&operation)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the operation's verified inclusions, dropping any whose block changed.
    fn confirm_inclusions(
        &self,
        operation: ExecutorOperationId,
        inclusions: Vec<(B256, ExecutorPayloadInclusion)>,
    ) {
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(operation, inclusions);
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, PoolSlot> {
        self.pool.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current_pool(&self) -> Option<Arc<QueryRpcPool>> {
        self.slot().pool.clone()
    }

    /// Drops the pool for good. Idempotent. Admission still in flight completes
    /// without publishing its pool.
    pub(super) fn release(&self) {
        let mut slot = self.slot();
        slot.released = true;
        slot.pool = None;
    }

    /// Admitted endpoints outside their cool-down, the preferred one first. Empty
    /// once released.
    pub(super) async fn providers(&self) -> Vec<ProviderHandle> {
        if self.slot().released {
            return Vec::new();
        }
        let started = Instant::now();
        tracing::debug!(
            target: "executor_observation",
            step = "endpoint_admission",
            initialized = self.admitted.initialized(),
            "started"
        );
        self.admitted
            .get_or_init(|| async {
                let pool = Arc::new(
                    QueryRpcPool::with_http_client(
                        self.route.endpoint_urls(),
                        ENDPOINT_COOLDOWN,
                        self.client.clone(),
                    )
                    .with_pending_admission(),
                );
                // Returns at the first admitted endpoint or once every first check
                // failed. The pool is kept either way and admits endpoints as they
                // pass, unless the owner released it meanwhile.
                let _ = self.route.admit_sync_pool(&self.client, &pool, None).await;
                let mut slot = self.slot();
                if !slot.released {
                    slot.pool = Some(pool);
                }
            })
            .await;
        let Some(pool) = self.current_pool() else {
            return Vec::new();
        };
        let mut providers = pool.available_providers();
        tracing::debug!(
            target: "executor_observation",
            step = "endpoint_admission",
            elapsed_ms = started.elapsed().as_millis(),
            available = providers.len(),
            "finished"
        );
        let preferred = self.preferred.load(Ordering::Relaxed);
        if let Some(position) = providers
            .iter()
            .position(|provider| provider.index == preferred)
        {
            providers[..=position].rotate_right(1);
        }
        providers
    }

    /// Endpoints admitted since an earlier [`Self::providers`] call: the available ones whose
    /// pool index isn't in `tried`, as soon as there is one, waiting until `deadline` while an
    /// untried endpoint's admission is pending. Empty at the deadline, once no such admission
    /// is pending, and once released.
    pub(super) async fn later_providers(
        &self,
        tried: &BTreeSet<usize>,
        deadline: tokio::time::Instant,
    ) -> Vec<ProviderHandle> {
        loop {
            // The pool is looked up on every pass and not held while waiting, so a release
            // still drops it.
            let pending = {
                let Some(pool) = self.current_pool() else {
                    return Vec::new();
                };
                // Read before the providers: an endpoint admitted between the two reads then
                // still counts as pending, and the next pass finds it.
                let pending = (0..pool.len()).any(|index| {
                    !tried.contains(&index)
                        && pool.provider_admission(index) == Some(RpcAdmission::Pending)
                });
                let mut providers = pool.available_providers();
                providers.retain(|provider| !tried.contains(&provider.index));
                if !providers.is_empty() {
                    return providers;
                }
                pending
            };
            let now = tokio::time::Instant::now();
            if !pending || now >= deadline {
                return Vec::new();
            }
            tokio::time::sleep_until(deadline.min(now + LATE_ADMISSION_POLL)).await;
        }
    }

    pub(super) fn succeeded(&self, provider: &ProviderHandle) {
        self.preferred.store(provider.index, Ordering::Relaxed);
    }

    /// Cools `provider` down when the endpoint failed rather than the observation:
    /// a transport or HTTP failure, an unusable response, or an error response
    /// other than a revert. Chain changes and incomplete history don't count.
    pub(super) fn failed(&self, provider: &ProviderHandle, error: &eyre::Report) {
        // RPC error text can contain response bodies, addresses, or endpoint credentials.
        let (failure, rpc_code) = match error.downcast_ref::<TransportError>() {
            Some(RpcError::Transport(_)) => ("transport", None),
            Some(RpcError::NullResp) => ("missing_response", None),
            Some(RpcError::DeserError { .. }) => ("decode", None),
            Some(RpcError::ErrorResp(payload)) => ("rpc_error", Some(payload.code)),
            Some(_) => ("rpc_client", None),
            None => ("observation", None),
        };
        tracing::debug!(
            target: "executor_observation",
            rpc_index = provider.index,
            failure,
            rpc_code,
            "observation failed"
        );
        let Some(pool) = self.current_pool() else {
            return;
        };
        let endpoint_failed =
            error
                .downcast_ref::<TransportError>()
                .is_some_and(|error| match error {
                    RpcError::Transport(_) | RpcError::NullResp | RpcError::DeserError { .. } => {
                        true
                    }
                    RpcError::ErrorResp(payload) => !payload.message.contains("revert"),
                    _ => false,
                });
        if !endpoint_failed {
            return;
        }
        let _ = self.preferred.compare_exchange(
            provider.index,
            usize::MAX,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        // Keep one endpoint to try, so a local outage can't stop every observation
        // for a whole cool-down.
        if pool.available_providers().len() > 1 {
            pool.mark_bad_provider(provider);
        }
    }
}

#[derive(Clone, Copy)]
enum NonceSource {
    Inspect,
    Signing(ExecutorNonceObservation),
    HistoryOnly,
}

pub(super) struct ExecutorHistoryObservation {
    pub(super) nonce: Option<ExecutorNonceObservation>,
    pub(super) block: alloy::eips::BlockNumHash,
    /// The executor's code at `block`, when the nonce read loaded it.
    pub(super) code: Option<Bytes>,
    pub(super) inclusions: Vec<(B256, ExecutorPayloadInclusion)>,
    pub(super) recovery_inclusions: Vec<(B256, ExecutorPayloadInclusion)>,
    /// Every inclusion this observation read in full or rechecked, still canonical.
    verified: Vec<(B256, ExecutorPayloadInclusion)>,
}

/// Scan explicit blocks, never query a private transaction hash at a remote endpoint.
/// Signing supplies its already checked nonce and canonical block. Reuse that
/// snapshot within this operation; other explicit reconciliation supplies `None`.
pub(super) async fn observe_executor_history(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
    signing_nonce: Option<ExecutorNonceObservation>,
) -> Result<ExecutorHistoryObservation> {
    observe_history(
        endpoints,
        chain,
        record,
        range,
        signing_nonce.map_or(NonceSource::Inspect, NonceSource::Signing),
    )
    .await
}

/// Private sync supplies a block location, not proof of executor execution.
/// Verify its receipt and expected effects without querying the account or hash.
pub(super) async fn observe_synced_executor_history(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    number: u64,
) -> Result<ExecutorHistoryObservation> {
    observe_history(
        endpoints,
        chain,
        record,
        number..number.saturating_add(1),
        NonceSource::HistoryOnly,
    )
    .await
}

async fn observe_history(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
    nonce_source: NonceSource,
) -> Result<ExecutorHistoryObservation> {
    if range.is_empty() || range.end - range.start > MAX_OBSERVATION_BLOCKS {
        return Err(eyre!(
            "executor observation requires between 1 and 64 blocks"
        ));
    }
    // With no locally issued payloads there are no identities for a history scan
    // to match. The signing inspection already checked the account and nonce.
    if let NonceSource::Signing(observed) = nonce_source
        && record.issued().is_empty()
        && record.recovery_transactions().is_empty()
        && range.end - 1 <= observed.block().number
    {
        return Ok(ExecutorHistoryObservation {
            nonce: Some(observed),
            block: observed.block(),
            code: None,
            inclusions: Vec::new(),
            recovery_inclusions: Vec::new(),
            verified: Vec::new(),
        });
    }
    let verified = endpoints.verified_inclusions(record.operation());
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match trace_step(
            "history_rpc",
            observe_history_at_provider(
                &provider.provider,
                chain,
                record,
                range.clone(),
                nonce_source,
                &verified,
            ),
        )
        .instrument(span)
        .await
        {
            Ok(mut observation) => {
                endpoints.succeeded(&provider);
                let confirmed = std::mem::take(&mut observation.verified);
                endpoints.confirm_inclusions(record.operation(), confirmed);
                return Ok(observation);
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!(
        "executor canonical history is unavailable; its issued payloads remain pending"
    ))
}

async fn observe_history_at_provider(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
    nonce_source: NonceSource,
    verified: &[(B256, ExecutorPayloadInclusion)],
) -> Result<ExecutorHistoryObservation> {
    let address = record
        .address()
        .ok_or_else(|| eyre!("executor address is unavailable"))?;
    let latest = trace_step("history_head", async { provider.get_block_number().await }).await?;
    let confirmed_tip = latest.saturating_sub(chain.finality_depth);
    let signing_nonce = match nonce_source {
        NonceSource::Signing(observed) => Some(observed),
        NonceSource::Inspect | NonceSource::HistoryOnly => None,
    };
    let confirmed_number = signing_nonce.map_or(confirmed_tip, |observed| observed.block().number);
    if confirmed_number > confirmed_tip || range.end - 1 > confirmed_number {
        return Err(eyre!(
            "requested executor history has not reached the configured confirmation depth"
        ));
    }
    // Counts only, showing whether the nonce read lies past the scanned page.
    tracing::debug!(
        target: "executor_observation",
        step = "history_range",
        blocks = range.end.saturating_sub(range.start),
        beyond_range = confirmed_number.saturating_sub(range.end - 1),
        "planned"
    );
    let confirmed = trace_step("history_confirmed_block", async {
        provider.get_block_by_number(confirmed_number.into()).await
    })
    .await?
    .ok_or_else(|| eyre!("confirmed executor block is unavailable"))?;
    let block = confirmed.header.num_hash();
    if block.number != confirmed_number {
        return Err(eyre!(
            "executor observation block does not match its requested height"
        ));
    }
    if signing_nonce.is_some_and(|observed| observed.block() != block) {
        return Err(eyre!("executor signing block is no longer canonical"));
    }
    let (nonce, code) = match nonce_source {
        NonceSource::Signing(observed) => (Some(observed.nonce()), None),
        NonceSource::Inspect => {
            let started = Instant::now();
            tracing::debug!(target: "executor_observation", step = "history_nonce", "started");
            // Keep the code that selects the nonce layout; a swap setup check reuses it.
            let block_id = alloy::eips::BlockId::hash_canonical(block.hash);
            let code = provider.get_code_at(address).block_id(block_id).await.ok();
            let nonce = super::executor_discovery::execution_nonce_with_code(
                provider,
                chain,
                address,
                block_id,
                code.as_ref().map(AsRef::as_ref),
                false,
            )
            .await;
            tracing::debug!(
                target: "executor_observation",
                step = "history_nonce",
                elapsed_ms = started.elapsed().as_millis(),
                available = nonce.is_some(),
                "finished"
            );
            (nonce, code)
        }
        NonceSource::HistoryOnly => (None, None),
    };
    let railgun = chain.require_railgun()?.deployment.contract;
    // Revalidate previous inclusions, even outside this discovery page. An old
    // cached winner must not survive a reorg or an unavailable block read. Only a
    // full canonical read proves an inclusion: a recorded one may rest on local
    // evidence, such as a sending endpoint's receipt, at a canonical block that
    // lacks the transaction. Once this session read a block in full, its receipts
    // stay fixed while its hash does, so the inclusions confirmed there stand after
    // a header read. Every other block is read in full: one with an inclusion not
    // confirmed in this session, a changed or unavailable one, one whose payload
    // inclusion still lacks the sender's account nonce while recovery transactions
    // may need that evidence, and one that private sync reported for a payload.
    let needs_account_nonce = !record.recovery_transactions().is_empty();
    let synced =
        |number: u64| matches!(nonce_source, NonceSource::HistoryOnly) && range.contains(&number);
    let mut revalidated = BTreeMap::<u64, bool>::new();
    for inclusion in record
        .issued()
        .iter()
        .filter_map(IssuedExecutorPayload::inclusion)
    {
        let number = inclusion.block().number;
        if number <= confirmed_number {
            *revalidated.entry(number).or_insert_with(|| synced(number)) |=
                needs_account_nonce && inclusion.executor_account_nonce().is_none();
        }
    }
    for inclusion in record
        .recovery_transactions()
        .iter()
        .filter_map(IssuedExecutorRecoveryTransaction::inclusion)
    {
        let number = inclusion.block().number;
        if number <= confirmed_number {
            revalidated.entry(number).or_insert_with(|| synced(number));
        }
    }
    let mut blocks = BTreeMap::new();
    for (number, mut full_read) in revalidated {
        full_read |=
            !recorded_inclusions(record, number).all(|recorded| verified.contains(&recorded));
        if !full_read {
            let current = if number == confirmed_number {
                Some(block)
            } else {
                trace_step("history_inclusion_header", async {
                    provider.get_block_by_number(number.into()).await
                })
                .await?
                .map(|current| current.header.num_hash())
            };
            full_read = current.is_none_or(|current| {
                recorded_inclusions(record, number)
                    .any(|(_, inclusion)| inclusion.block() != current)
            });
        }
        let observed = if full_read {
            trace_step(
                "history_inclusion_block",
                observe_block(provider, railgun, record, number),
            )
            .await?
        } else {
            recorded_block(record, number)
        };
        blocks.insert(number, observed);
    }
    let scan_range = nonce.is_none_or(|nonce| {
        let (inclusions, recovery_inclusions) = merge_observed_blocks(blocks.values());
        range_scan_can_find_executions(record, nonce, &inclusions, &recovery_inclusions)
    });
    if scan_range {
        for number in range {
            if let std::collections::btree_map::Entry::Vacant(entry) = blocks.entry(number) {
                entry.insert(
                    trace_step(
                        "history_scan_block",
                        observe_block(provider, railgun, record, number),
                    )
                    .await?,
                );
            }
        }
    }
    let (inclusions, recovery_inclusions) = merge_observed_blocks(blocks.values());
    let still_canonical = trace_step("history_canonical_recheck", async {
        provider.get_block_by_number(confirmed_number.into()).await
    })
    .await?
    .is_some_and(|current| current.header.num_hash() == block);
    if !still_canonical {
        return Err(eyre!("executor chain changed during observation"));
    }
    Ok(ExecutorHistoryObservation {
        nonce: nonce.map(|nonce| ExecutorNonceObservation::new(block, nonce)),
        block,
        code,
        inclusions: inclusions.into_iter().collect(),
        recovery_inclusions,
        verified: blocks
            .values()
            .flat_map(|observed| observed.execution.iter().chain(&observed.recovery))
            .copied()
            .collect(),
    })
}

/// Merge in block order. An executed inclusion outranks any other of the same payload.
fn merge_observed_blocks<'a>(
    blocks: impl Iterator<Item = &'a ObservedExecutorBlock>,
) -> (
    BTreeMap<B256, ExecutorPayloadInclusion>,
    Vec<(B256, ExecutorPayloadInclusion)>,
) {
    let mut inclusions = BTreeMap::new();
    let mut recovery_inclusions = Vec::new();
    for observed in blocks {
        recovery_inclusions.extend(observed.recovery.iter().copied());
        for (hash, inclusion) in &observed.execution {
            let previous = inclusions.get(hash).copied();
            if previous.is_none_or(|previous: ExecutorPayloadInclusion| {
                previous.result() != ExecutorExecutionResult::Executed
            }) {
                inclusions.insert(*hash, *inclusion);
            }
        }
    }
    (inclusions, recovery_inclusions)
}

/// Whether a block scan for direct executor calls could find anything that the
/// execution `nonce` at the confirmed block and the revalidated inclusions leave
/// unexplained. Each block costs a full-transaction read, so a page is skipped
/// when this is false and the caller still treats it as covered.
///
/// A successful direct call consumes its payload's nonce, so a winner can hide
/// in the page only at a consumed nonce that no executed inclusion and no swap
/// hook observation accounts for. Swap hooks run inside settlements, not as
/// direct calls, and swap observation finds them. Recovery transactions are
/// plain executor-account transactions with no execution nonce to compare, so
/// any without an inclusion forces the scan.
///
/// Tradeoff: an attempt that reverted in a skipped page leaves the nonce
/// unchanged and is not recorded as reverted. Its payload stays pending and
/// keeps its inputs reserved, which is the conservative outcome. Private sync
/// separately reports spend locations for executed payloads.
fn range_scan_can_find_executions(
    record: &ExecutorRecord,
    nonce: U256,
    inclusions: &BTreeMap<B256, ExecutorPayloadInclusion>,
    recovery_inclusions: &[(B256, ExecutorPayloadInclusion)],
) -> bool {
    let explained = |consumed: U256| {
        record.issued().iter().any(|payload| {
            payload.nonce() == consumed
                && inclusions.get(&payload.hash()).is_some_and(|inclusion| {
                    inclusion.result() == ExecutorExecutionResult::Executed
                })
        }) || record.swap().is_some_and(|swap| {
            swap.orders().iter().any(|order| {
                let observed = order.observations();
                (order.pre_hook().nonce() == consumed && observed.pre_hook_executed.is_some())
                    || (order
                        .post_hook()
                        .is_some_and(|hook| hook.nonce() == consumed)
                        && observed.post_hook_evidence())
            })
        })
    };
    record.issued().iter().any(|payload| {
        matches!(
            payload.purpose(),
            ExecutorPayloadPurpose::Operation | ExecutorPayloadPurpose::Recovery
        ) && payload.nonce() < nonce
            && !explained(payload.nonce())
    }) || record.recovery_transactions().iter().any(|transaction| {
        !recovery_inclusions
            .iter()
            .any(|(hash, _)| *hash == transaction.hash())
    })
}

#[derive(Default)]
struct ObservedExecutorBlock {
    execution: Vec<(B256, ExecutorPayloadInclusion)>,
    recovery: Vec<(B256, ExecutorPayloadInclusion)>,
}

/// The recorded payload and recovery inclusions at `number`, keyed as observed.
fn recorded_inclusions(
    record: &ExecutorRecord,
    number: u64,
) -> impl Iterator<Item = (B256, ExecutorPayloadInclusion)> + '_ {
    recorded_payload_inclusions(record, number).chain(recorded_recovery_inclusions(record, number))
}

fn recorded_payload_inclusions(
    record: &ExecutorRecord,
    number: u64,
) -> impl Iterator<Item = (B256, ExecutorPayloadInclusion)> + '_ {
    record.issued().iter().filter_map(move |payload| {
        payload
            .inclusion()
            .filter(|inclusion| inclusion.block().number == number)
            .map(|inclusion| (payload.hash(), inclusion))
    })
}

fn recorded_recovery_inclusions(
    record: &ExecutorRecord,
    number: u64,
) -> impl Iterator<Item = (B256, ExecutorPayloadInclusion)> + '_ {
    record
        .recovery_transactions()
        .iter()
        .filter_map(move |transaction| {
            transaction
                .inclusion()
                .filter(|inclusion| inclusion.block().number == number)
                .map(|inclusion| (transaction.hash(), inclusion))
        })
}

/// The recorded outcomes at `number`, for a block that this session read in full
/// and that still has the same hash.
fn recorded_block(record: &ExecutorRecord, number: u64) -> ObservedExecutorBlock {
    ObservedExecutorBlock {
        execution: recorded_payload_inclusions(record, number).collect(),
        recovery: recorded_recovery_inclusions(record, number).collect(),
    }
}

async fn observe_block(
    provider: &DynProvider,
    railgun: Address,
    record: &ExecutorRecord,
    number: u64,
) -> Result<ObservedExecutorBlock> {
    // Full blocks and their receipts can contain chain-specific transaction types
    // (e.g. Arbitrum system transactions). Keep the admitted, privacy-routed client.
    let block =
        EthGetBlock::<alloy::network::AnyRpcBlock>::by_number(number.into(), provider.client())
            .full()
            .await?
            .ok_or_else(|| eyre!("executor observation block is unavailable"))?;
    let identity = block.header.num_hash();
    if identity.number != number {
        return Err(eyre!("executor block identity mismatch"));
    }
    let BlockTransactions::Full(transactions) = &block.transactions else {
        return Err(eyre!("executor observation needs full block transactions"));
    };
    let mut matches = Vec::new();
    let mut recovery_matches = Vec::new();
    for transaction in transactions {
        for issued in record.recovery_transactions() {
            if recovery::matches_transaction(issued, transaction) {
                recovery_matches.push(issued);
            }
        }
        if transaction.to() != record.address() {
            continue;
        }
        for payload in record.issued() {
            if transaction.input() == payload.context().calldata() {
                let account_nonce =
                    (Some(transaction.from()) == record.address()).then_some(transaction.nonce());
                matches.push((transaction.tx_hash(), payload, account_nonce));
            }
        }
    }
    if matches.is_empty() && recovery_matches.is_empty() {
        return Ok(ObservedExecutorBlock::default());
    }
    let hashes = transactions
        .iter()
        .map(alloy::network::TransactionResponse::tx_hash)
        .collect::<Vec<_>>();
    let receipts = fetch_checked_block_receipts(provider, identity, &hashes)
        .await
        .map_err(|_| eyre!("executor block receipts are incomplete or unavailable"))?;
    let mut observed = ObservedExecutorBlock::default();
    for (hash, payload, account_nonce) in matches {
        let receipt = receipts
            .iter()
            .find(|receipt| receipt.transaction_hash == hash)
            .ok_or_else(|| eyre!("executor receipt is absent from its block"))?;
        let result = execution_effects(
            railgun,
            record.address().expect("matched executor"),
            payload,
            receipt,
        )?;
        observed.execution.push((
            payload.hash(),
            ExecutorPayloadInclusion::new(identity, hash, result)
                .with_executor_account_nonce(account_nonce),
        ));
    }
    for issued in recovery_matches {
        let receipt = receipts
            .iter()
            .find(|receipt| receipt.transaction_hash == issued.hash())
            .ok_or_else(|| eyre!("recovery receipt is absent from its block"))?;
        let result = recovery::effects(railgun, issued, receipt)?;
        observed.recovery.push((
            issued.hash(),
            ExecutorPayloadInclusion::new(identity, issued.hash(), result),
        ));
    }
    // A block fetched before a reorg may still be retrievable by hash afterward.
    if provider
        .get_block_by_number(number.into())
        .await?
        .is_none_or(|current| current.header.num_hash() != identity)
    {
        return Err(eyre!("executor receipt lost canonical inclusion"));
    }
    Ok(observed)
}

fn execution_effects(
    railgun: Address,
    executor: Address,
    payload: &IssuedExecutorPayload,
    receipt: &TransactionReceipt<impl TxReceipt<Log = Log>>,
) -> Result<ExecutorExecutionResult> {
    if !receipt.inner.status() {
        return Ok(ExecutorExecutionResult::Reverted);
    }
    let data = payload.context().calldata();
    let (transactions, calls) = if let Ok(call) = RelayAdapt7702::executeCall::abi_decode(data) {
        if call._nonce != payload.nonce() || !call._actionData.requireSuccess {
            return Err(eyre!(
                "recorded executor payload does not require exact successful execution"
            ));
        }
        (call._transactions, call._actionData.calls)
    } else {
        let call = RelayAdapt7702::multicallCall::abi_decode(data)?;
        if call._nonce != payload.nonce() || !call._requireSuccess {
            return Err(eyre!(
                "recorded recovery payload does not require exact successful execution"
            ));
        }
        (Vec::new(), call._calls)
    };
    let shields = expected_shields(executor, railgun, &calls)?;
    // An empty successful call cannot identify a winner from nonce advancement.
    if transactions
        .iter()
        .all(|transaction| transaction.nullifiers.is_empty())
        && shields.is_empty()
    {
        return Ok(ExecutorExecutionResult::MissingEffects);
    }
    if !private_effects_present(railgun, &transactions, receipt.logs())
        || !shield_effects_present(railgun, &shields, receipt)
    {
        return Ok(ExecutorExecutionResult::MissingEffects);
    }
    Ok(ExecutorExecutionResult::Executed)
}

pub(super) fn expected_shields(
    executor: Address,
    railgun: Address,
    calls: &[Call],
) -> Result<Vec<ShieldRequest>> {
    let mut requests = Vec::new();
    for call in calls {
        if (call.to == executor || call.to == railgun)
            && call.data.starts_with(&shieldCall::SELECTOR)
        {
            requests.extend(shieldCall::abi_decode(&call.data)?._shieldRequests);
        }
    }
    Ok(requests)
}

/// Every nullifier and ciphertext-bearing commitment of `transactions` appears in
/// `logs`, which come from one successful transaction.
pub(super) fn private_effects_present(
    railgun: Address,
    transactions: &[Transaction],
    logs: &[Log],
) -> bool {
    let mut nullifiers = BTreeSet::new();
    let mut commitments = BTreeSet::new();
    for log in logs
        .iter()
        .filter(|log| log.address() == railgun && !log.removed)
    {
        if let Ok(event) = log.log_decode::<Nullified>() {
            nullifiers.extend(
                event
                    .inner
                    .data
                    .nullifier
                    .into_iter()
                    .map(|nullifier| (event.inner.data.treeNumber, nullifier)),
            );
        }
        if let Ok(event) = log.log_decode::<Transact>() {
            commitments.extend(event.inner.data.hash);
        }
    }
    transactions.iter().all(|transaction| {
        transaction
            .nullifiers
            .iter()
            .all(|nullifier| nullifiers.contains(&(transaction.boundParams.treeNumber, *nullifier)))
            && transaction
                .commitments
                .iter()
                .take(transaction.boundParams.commitmentCiphertext.len())
                .all(|commitment| commitments.contains(commitment))
    })
}

fn shield_effects_present(
    railgun: Address,
    requests: &[ShieldRequest],
    receipt: &TransactionReceipt<impl TxReceipt<Log = Log>>,
) -> bool {
    let mut observed = Vec::new();
    for log in receipt
        .logs()
        .iter()
        .filter(|log| log.address() == railgun && !log.removed)
    {
        if let Ok(event) = log.log_decode::<Shield>() {
            let event = event.inner.data;
            if event.commitments.len() != event.shieldCiphertext.len()
                || event.commitments.len() != event.fees.len()
            {
                return false;
            }
            observed.extend(
                event
                    .commitments
                    .into_iter()
                    .zip(event.shieldCiphertext)
                    .zip(event.fees),
            );
        }
    }
    requests.iter().all(|request| {
        observed
            .iter()
            .position(|((preimage, ciphertext), fee)| {
                preimage.npk == request.preimage.npk
                    && preimage.token.abi_encode() == request.preimage.token.abi_encode()
                    && U256::from(preimage.value).checked_add(*fee)
                        == Some(U256::from(request.preimage.value))
                    && ciphertext.abi_encode() == request.ciphertext.abi_encode()
            })
            .is_some_and(|index| {
                observed.remove(index);
                true
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::ExecutorPayloadContext;
    use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{Bytes, Uint};
    use alloy::sol_types::SolEvent;
    use broadcaster_core::contracts::railgun::{
        BoundParams, CommitmentCiphertext, CommitmentPreimage, RelayAdapt7702ActionData,
        SnarkProof, TokenData,
    };
    use broadcaster_core::contracts::shield::build_shield_request;

    fn empty_record(chain: &EffectiveChainConfig) -> ExecutorRecord {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "derivation": "Railgun7702V1",
            "origin": "Reserved",
            "operation": crate::vault::ExecutorOperationId::random().unwrap(),
            "index": 0,
            "address": Address::repeat_byte(1),
            "delegate": chain.accepted_executor_profile().unwrap().delegate(),
            "retired": false,
            "created_at": null,
            "restored_at": null,
            "purpose_summary": null,
            "assets": [],
            "hidden": false,
            "issued": [],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn signing_without_issued_history_reuses_observation_without_rpc() {
        use crate::vault::{ExecutorRecoveryStepKind, IssuedExecutorRecoveryTransaction};
        let mut chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(1, Vec::<url::Url>::new());
        let endpoints = ObservationEndpoints::new(&chain, &HttpContext::direct_for_tests());
        let record = empty_record(&chain);
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(50, B256::repeat_byte(50)), U256::ZERO);
        let history = observe_executor_history(&endpoints, &chain, &record, 50..51, Some(observed))
            .await
            .unwrap();
        assert_eq!(history.nonce, Some(observed));
        assert!(history.inclusions.is_empty());
        assert!(history.recovery_inclusions.is_empty());
        // Neither an absent observation nor any previously issued payload can
        // use the offline branch, including ordinary recovery transactions.
        assert!(
            observe_executor_history(&endpoints, &chain, &record, 50..51, None)
                .await
                .is_err()
        );
        let payload = IssuedExecutorPayload::new(
            U256::ZERO,
            record.delegate(),
            B256::repeat_byte(1),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(Bytes::from_static(&[1]), observed, Vec::new()),
        );
        let recovery = IssuedExecutorRecoveryTransaction::new(
            record.operation(),
            0,
            ExecutorRecoveryStepKind::Wrap,
            alloy::rpc::types::TransactionRequest::default(),
            B256::repeat_byte(2),
            observed.block(),
        );
        for (field, values) in [
            ("issued", serde_json::json!([payload])),
            ("recovery_transactions", serde_json::json!([recovery])),
        ] {
            let mut value = serde_json::to_value(&record).unwrap();
            value[field] = values;
            let previous = serde_json::from_value(value).unwrap();
            assert!(
                observe_executor_history(&endpoints, &chain, &previous, 50..51, Some(observed),)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn signing_history_stays_on_its_confirmed_block_and_rejects_reorgs() {
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let record = empty_record(&chain);
        let pinned = BlockNumHash::new(50, B256::repeat_byte(50));
        for reorg in [false, true] {
            let responses = Asserter::new();
            let provider = ProviderBuilder::new()
                .connect_mocked_client(responses.clone())
                .erased();
            let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
            block.header.inner.number = pinned.number;
            block.header.hash = if reorg {
                B256::repeat_byte(51)
            } else {
                pinned.hash
            };
            block.transactions = BlockTransactions::Full(Vec::new());
            // A newer confirmed tip must not move the signing snapshot.
            responses.push_success(&format!("0x{:x}", pinned.number + chain.finality_depth + 2));
            responses.push_success(&block);
            // Signing already obtained the nonce at this exact block, and with
            // nothing issued there is no page to scan.
            responses.push_success(&block);
            let result = observe_history_at_provider(
                &provider,
                &chain,
                &record,
                50..51,
                NonceSource::Signing(ExecutorNonceObservation::new(pinned, U256::ZERO)),
                &[],
            )
            .await;
            if reorg {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap().nonce,
                    Some(ExecutorNonceObservation::new(pinned, U256::ZERO))
                );
            }
        }
    }

    #[tokio::test]
    async fn synced_history_requires_confirmed_canonical_blocks_without_a_nonce_read() {
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let record = empty_record(&chain);
        for (confirmed, reorg) in [(true, false), (false, false), (true, true)] {
            let responses = Asserter::new();
            let provider = ProviderBuilder::new()
                .connect_mocked_client(responses.clone())
                .erased();
            let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
            block.header.inner.number = 50;
            block.header.hash = B256::repeat_byte(50);
            block.transactions = BlockTransactions::Full(Vec::new());
            responses.push_success(&format!(
                "0x{:x}",
                50 + chain.finality_depth - u64::from(!confirmed)
            ));
            // Only the anchor, full block, and canonicality recheck are available.
            // An account nonce query would consume a block response and fail.
            responses.push_success(&block);
            responses.push_success(&block);
            if reorg {
                block.header.hash = B256::repeat_byte(51);
            }
            responses.push_success(&block);
            let result = observe_history_at_provider(
                &provider,
                &chain,
                &record,
                50..51,
                NonceSource::HistoryOnly,
                &[],
            )
            .await;
            if confirmed && !reorg {
                assert!(result.unwrap().nonce.is_none());
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[tokio::test]
    async fn history_skips_full_blocks_unless_a_consumed_nonce_or_recovery_is_unexplained() {
        use crate::vault::{ExecutorRecoveryStepKind, IssuedExecutorRecoveryTransaction};
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let record = empty_record(&chain);
        let signed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::ZERO), U256::from(5));
        // A stuck operation at nonce 5 whose earlier attempt reverted in block 30.
        let mut pending = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(5),
            record.delegate(),
            B256::repeat_byte(1),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(Bytes::from_static(&[1]), signed, Vec::new()),
        ))
        .unwrap();
        pending["inclusion"] = serde_json::to_value(ExecutorPayloadInclusion::new(
            BlockNumHash::new(30, B256::repeat_byte(30)),
            B256::repeat_byte(2),
            ExecutorExecutionResult::Reverted,
        ))
        .unwrap();
        let recovery = IssuedExecutorRecoveryTransaction::new(
            record.operation(),
            0,
            ExecutorRecoveryStepKind::Wrap,
            alloy::rpc::types::TransactionRequest::default(),
            B256::repeat_byte(3),
            signed.block(),
        );
        let block_at = |number: u64| {
            let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
            block.header.inner.number = number;
            block.header.hash = B256::repeat_byte(u8::try_from(number).unwrap());
            block.transactions = BlockTransactions::Full(Vec::new());
            block
        };
        let range = 40..48;
        // An unconsumed nonce cannot hide a winner in the page. A consumed one
        // without an executed inclusion, or an unincluded recovery transaction, can.
        for (current, recovering, scanned) in [(5, false, false), (6, false, true), (5, true, true)]
        {
            let mut value = serde_json::to_value(&record).unwrap();
            value["issued"] = serde_json::json!([pending]);
            if recovering {
                value["recovery_transactions"] = serde_json::json!([recovery]);
            }
            let previous: ExecutorRecord = serde_json::from_value(value).unwrap();
            let responses = Asserter::new();
            let provider = ProviderBuilder::new()
                .connect_mocked_client(responses.clone())
                .erased();
            responses.push_success(&format!("0x{:x}", 50 + chain.finality_depth));
            responses.push_success(&block_at(50));
            // An undelegated executor exposes its execution nonce in storage.
            responses.push_success(&Bytes::new());
            responses.push_success(&U256::from(current));
            // The recorded inclusion is revalidated either way.
            responses.push_success(&block_at(30));
            if scanned {
                for number in range.clone() {
                    responses.push_success(&block_at(number));
                }
            }
            responses.push_success(&block_at(50));
            let history = observe_history_at_provider(
                &provider,
                &chain,
                &previous,
                range.clone(),
                NonceSource::Inspect,
                &[],
            )
            .await
            .unwrap();
            assert!(responses.read_q().is_empty());
            assert_eq!(
                history.nonce,
                Some(ExecutorNonceObservation::new(
                    BlockNumHash::new(50, B256::repeat_byte(50)),
                    U256::from(current)
                ))
            );
            assert!(history.inclusions.is_empty());
            assert!(history.recovery_inclusions.is_empty());
        }
    }

    #[tokio::test]
    async fn only_inclusions_read_in_full_this_session_are_rechecked_by_header() {
        use alloy::consensus::transaction::Recovered;
        use alloy::consensus::{SignableTransaction as _, TxEip1559, TxEnvelope};
        use alloy::primitives::{Signature, TxKind};
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let record = empty_record(&chain);
        let executor = record.address().unwrap();
        let signed =
            ExecutorNonceObservation::new(BlockNumHash::new(20, B256::ZERO), U256::from(5));
        let payload = IssuedExecutorPayload::new(
            U256::from(5),
            record.delegate(),
            B256::repeat_byte(1),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(Bytes::from_static(&[1]), signed, Vec::new()),
        );
        // The executor itself sent the payload in block 30, and it reverted.
        let included = BlockNumHash::new(30, B256::repeat_byte(30));
        let sent = TxEip1559 {
            chain_id: 1,
            nonce: 7,
            gas_limit: 1,
            max_fee_per_gas: 1,
            to: TxKind::Call(executor),
            input: Bytes::from_static(&[1]),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature());
        let sent_hash = *sent.hash();
        let mut block_30 = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block_30.header.inner.number = included.number;
        block_30.header.hash = included.hash;
        block_30.transactions = BlockTransactions::Full(vec![alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(TxEnvelope::Eip1559(sent), executor),
            block_hash: Some(included.hash),
            block_number: Some(included.number),
            transaction_index: Some(0),
            effective_gas_price: Some(1),
            block_timestamp: None,
        }]);
        let mut reverted = receipt(false, Vec::new());
        reverted.transaction_hash = sent_hash;
        reverted.block_hash = Some(included.hash);
        reverted.block_number = Some(included.number);
        let inclusion =
            ExecutorPayloadInclusion::new(included, sent_hash, ExecutorExecutionResult::Reverted)
                .with_executor_account_nonce(Some(7));
        let mut value = serde_json::to_value(&record).unwrap();
        let mut issued = serde_json::to_value(&payload).unwrap();
        issued["inclusion"] = serde_json::to_value(inclusion).unwrap();
        value["issued"] = serde_json::json!([issued]);
        let previous: ExecutorRecord = serde_json::from_value(value).unwrap();
        let block_at = |number: u64, hash: u8| {
            let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
            block.header.inner.number = number;
            block.header.hash = B256::repeat_byte(hash);
            block.transactions = BlockTransactions::Full(Vec::new());
            block
        };
        let observe = |block_30: Vec<serde_json::Value>,
                       verified: Vec<(B256, ExecutorPayloadInclusion)>| {
            let chain = &chain;
            let previous = &previous;
            async move {
                let responses = Asserter::new();
                let provider = ProviderBuilder::new()
                    .connect_mocked_client(responses.clone())
                    .erased();
                responses.push_success(&format!("0x{:x}", 50 + chain.finality_depth));
                responses.push_success(&block_at(50, 50));
                // Nonce 5 is still unused, so no page scan follows.
                responses.push_success(&Bytes::new());
                responses.push_success(&U256::from(5));
                for response in &block_30 {
                    responses.push_success(response);
                }
                responses.push_success(&block_at(50, 50));
                let history = observe_history_at_provider(
                    &provider,
                    chain,
                    previous,
                    50..51,
                    NonceSource::Inspect,
                    &verified,
                )
                .await
                .unwrap();
                assert!(responses.read_q().is_empty());
                history
            }
        };
        let full_block = serde_json::to_value(&block_30).unwrap();
        // The first observation this session reads the block, its receipts, and its
        // canonicality in full, then keeps the inclusion with its sender nonce.
        let first = observe(
            vec![
                full_block.clone(),
                serde_json::to_value(vec![reverted]).unwrap(),
                full_block.clone(),
            ],
            Vec::new(),
        )
        .await;
        assert_eq!(first.inclusions, vec![(payload.hash(), inclusion)]);
        // Once read in full, the same block is rechecked by one header read.
        let second = observe(vec![full_block], first.verified).await;
        assert_eq!(second.inclusions, vec![(payload.hash(), inclusion)]);
        // A changed block is read in full again, and the inclusion goes with it.
        let reorged = serde_json::to_value(block_at(30, 31)).unwrap();
        let third = observe(vec![reorged.clone(), reorged], second.verified).await;
        assert!(third.inclusions.is_empty());
    }

    #[tokio::test]
    async fn observes_executor_inclusion_alongside_arbitrum_system_transactions() {
        use alloy::consensus::transaction::Recovered;
        use alloy::consensus::{SignableTransaction as _, TxEip1559, TxEnvelope};
        use alloy::primitives::{Signature, TxKind};
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        use serde_json::json;

        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(42161)
        .cloned()
        .unwrap();
        let record = empty_record(&chain);
        let executor = record.address().unwrap();
        let included = BlockNumHash::new(30, B256::repeat_byte(30));
        let payload = issued(vec![1]);
        let mut value = serde_json::to_value(&record).unwrap();
        value["issued"] = json!([payload]);
        let record = serde_json::from_value(value).unwrap();
        let sent = TxEip1559 {
            chain_id: chain.chain_id,
            nonce: 7,
            gas_limit: 1,
            max_fee_per_gas: 1,
            to: TxKind::Call(executor),
            input: Bytes::from_static(&[1]),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature());
        let sent_hash = *sent.hash();
        let transaction = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(TxEnvelope::Eip1559(sent), executor),
            block_hash: Some(included.hash),
            block_number: Some(included.number),
            transaction_index: Some(1),
            effective_gas_price: Some(1),
            block_timestamp: None,
        };
        let system_hash = B256::repeat_byte(99);
        let mut header: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
        header.header.inner.number = included.number;
        header.header.hash = included.hash;
        let mut block = serde_json::to_value(&header).unwrap();
        // Arbitrum's internal transaction (0x6a) is not an Ethereum envelope.
        block["transactions"] = json!([
            {
                "type": "0x6a", "hash": system_hash, "from": Address::ZERO,
                "to": Address::repeat_byte(100), "input": "0x", "nonce": "0x0",
                "gas": "0x0", "gasPrice": "0x0", "value": "0x0",
                "blockHash": included.hash, "blockNumber": "0x1e", "transactionIndex": "0x0"
            },
            transaction
        ]);
        let mut reverted = receipt(false, Vec::new());
        reverted.transaction_hash = sent_hash;
        reverted.block_hash = Some(included.hash);
        reverted.block_number = Some(included.number);
        let mut system_receipt = serde_json::to_value(&reverted).unwrap();
        system_receipt["type"] = json!("0x6a");
        system_receipt["transactionHash"] = json!(system_hash);
        system_receipt["status"] = json!("0x1");
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        responses.push_success(&block);
        responses.push_success(&json!([system_receipt, reverted]));
        responses.push_success(&header);
        let observed = observe_block(
            &provider,
            chain.require_railgun().unwrap().deployment.contract,
            &record,
            included.number,
        )
        .await
        .unwrap();
        assert_eq!(
            observed.execution,
            vec![(
                payload.hash(),
                ExecutorPayloadInclusion::new(
                    included,
                    sent_hash,
                    ExecutorExecutionResult::Reverted
                )
                .with_executor_account_nonce(Some(7)),
            )]
        );
        assert!(responses.read_q().is_empty());
    }

    #[tokio::test]
    async fn a_failed_endpoint_sits_out_and_a_verified_one_is_not_rechecked() {
        use serde_json::{Value, json};
        let mut chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 50;
        block.header.hash = B256::repeat_byte(50);
        let head = format!("0x{:x}", 50 + chain.finality_depth);
        let logged = |methods: &Arc<Mutex<Vec<String>>>, request: &Value| {
            let method = request["method"].as_str().unwrap().to_owned();
            methods.lock().unwrap().push(method.clone());
            method
        };
        let failing_methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = failing_methods.clone();
        let (failing, failing_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| {
                if logged(&recorded, &request) == "eth_chainId" {
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": -32000, "message": "payment required"}
                    })
                }
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let serving_methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = serving_methods.clone();
        let (serving, serving_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| {
                let result = match logged(&recorded, &request).as_str() {
                    "eth_chainId" => json!("0x1"),
                    "eth_blockNumber" => json!(head),
                    "eth_getBlockByNumber" => serde_json::to_value(&block).unwrap(),
                    "eth_getCode" => json!("0x"),
                    "eth_getStorageAt" => json!(B256::ZERO),
                    method => panic!("unexpected RPC method {method}"),
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        chain.rpc_route =
            crate::RpcChainRoute::new(1, vec![failing, serving]).with_identity_verification();
        let endpoints = ObservationEndpoints::new(&chain, &HttpContext::direct_for_tests());
        // Admission returns at the first verified endpoint and admits the other after.
        tokio::time::timeout(Duration::from_secs(5), async {
            while endpoints.providers().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let record = empty_record(&chain);
        for _ in 0..2 {
            let history = observe_executor_history(&endpoints, &chain, &record, 50..51, None)
                .await
                .unwrap();
            assert_eq!(history.block, BlockNumHash::new(50, B256::repeat_byte(50)));
        }
        // The first observation tried the failing endpoint once. The second skipped it.
        assert_eq!(
            *failing_methods.lock().unwrap(),
            vec!["eth_chainId", "eth_blockNumber"]
        );
        let served = serving_methods.lock().unwrap().clone();
        let count = |method: &str| served.iter().filter(|call| *call == method).count();
        assert_eq!((count("eth_chainId"), count("eth_blockNumber")), (1, 2));

        // Releasing drops the pool, so admission's weak reference can't keep it, and
        // nothing rebuilds it.
        let pool = Arc::downgrade(&endpoints.current_pool().unwrap());
        endpoints.release();
        endpoints.release();
        assert!(endpoints.providers().await.is_empty());
        assert!(pool.upgrade().is_none());
        assert!(endpoints.current_pool().is_none());

        // A release while the first admission is pending wins over its completion.
        let pending_endpoints = ObservationEndpoints::new(&chain, &HttpContext::direct_for_tests());
        let mut pending = std::pin::pin!(pending_endpoints.providers());
        assert!(futures_util::poll!(&mut pending).is_pending());
        pending_endpoints.release();
        assert!(pending.await.is_empty());
        assert!(pending_endpoints.current_pool().is_none());
        assert!(pending_endpoints.providers().await.is_empty());
        failing_server.abort();
        serving_server.abort();
    }

    fn event_log(event: &impl SolEvent, address: Address) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: event.encode_log_data(),
            },
            ..Default::default()
        }
    }

    fn receipt(status: bool, logs: Vec<Log>) -> TransactionReceipt {
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip7702(
                Receipt {
                    status: Eip658Value::Eip658(status),
                    cumulative_gas_used: 1,
                    logs,
                }
                .with_bloom(),
            ),
            transaction_hash: B256::repeat_byte(1),
            transaction_index: Some(0),
            block_hash: Some(B256::repeat_byte(11)),
            block_number: Some(11),
            gas_used: 1,
            effective_gas_price: 1,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::repeat_byte(1),
            to: Some(Address::repeat_byte(2)),
            contract_address: None,
        }
    }

    fn issued(data: Vec<u8>) -> IssuedExecutorPayload {
        // The effect checker consumes an already-issued call. Signature/proof validity
        // is covered by the signing and chain tests, not these synthetic event fixtures.
        IssuedExecutorPayload::new(
            U256::from(3),
            Address::repeat_byte(4),
            B256::repeat_byte(5),
            ExecutorPayloadPurpose::Recovery,
            ExecutorPayloadContext::new(
                data.into(),
                ExecutorNonceObservation::new(
                    BlockNumHash::new(10, B256::repeat_byte(10)),
                    U256::from(3),
                ),
                Vec::new(),
            ),
        )
    }

    fn shield(token: TokenData, amount: u64) -> ShieldRequest {
        let keys = railgun_wallet::ViewingKeyData::from_spending_public_key(
            [7; 32],
            [U256::ONE, U256::from(2)],
        );
        build_shield_request(
            keys.master_public_key,
            &keys.viewing_public_key,
            token,
            Uint::<120, 2>::from(amount),
            &[3; 32],
        )
        .unwrap()
    }

    #[test]
    fn executor_ordinary_recovery_requires_approval_and_wrap_effects() {
        use crate::desktop::executor_discovery::ExecutorErc721;
        use crate::public_wallet::PublicErc20;
        use crate::vault::{
            ExecutorOperationId, ExecutorRecoveryStepKind, IssuedExecutorRecoveryTransaction,
        };
        use crate::walletconnect::WrappedNative;
        use alloy::rpc::types::TransactionRequest;
        use broadcaster_core::contracts::railgun::approveCall;
        let source = Address::repeat_byte(1);
        let token = Address::repeat_byte(2);
        let railgun = Address::repeat_byte(3);
        let value = U256::from(42);
        let cases = [
            (
                ExecutorRecoveryStepKind::ApproveErc20,
                approveCall {
                    spender: railgun,
                    amount: value,
                }
                .abi_encode(),
                U256::ZERO,
                event_log(
                    &PublicErc20::Approval {
                        owner: source,
                        spender: railgun,
                        value,
                    },
                    token,
                ),
            ),
            (
                ExecutorRecoveryStepKind::ApproveErc721,
                ExecutorErc721::approveCall {
                    spender: railgun,
                    tokenId: value,
                }
                .abi_encode(),
                U256::ZERO,
                event_log(
                    &ExecutorErc721::Approval {
                        owner: source,
                        approved: railgun,
                        tokenId: value,
                    },
                    token,
                ),
            ),
            (
                ExecutorRecoveryStepKind::Wrap,
                WrappedNative::depositCall {}.abi_encode(),
                value,
                event_log(
                    &WrappedNative::Deposit {
                        dst: source,
                        wad: value,
                    },
                    token,
                ),
            ),
        ];
        for (kind, data, value, log) in cases {
            let mut request = TransactionRequest::default()
                .to(token)
                .input(data.into())
                .value(value);
            request.from = Some(source);
            let issued = IssuedExecutorRecoveryTransaction::new(
                ExecutorOperationId::random().unwrap(),
                0,
                kind,
                request,
                B256::repeat_byte(4),
                BlockNumHash::new(10, B256::repeat_byte(10)),
            );
            assert_eq!(
                recovery::effects(railgun, &issued, &receipt(true, vec![])).unwrap(),
                ExecutorExecutionResult::MissingEffects
            );
            assert_eq!(
                recovery::effects(railgun, &issued, &receipt(true, vec![log.clone()])).unwrap(),
                ExecutorExecutionResult::Executed
            );
            let mut wrong_emitter = log;
            wrong_emitter.inner.address = railgun;
            assert_eq!(
                recovery::effects(railgun, &issued, &receipt(true, vec![wrong_emitter])).unwrap(),
                ExecutorExecutionResult::MissingEffects
            );
        }
    }

    #[test]
    fn executor_receipt_requires_private_and_exact_shield_effects() {
        let railgun = Address::repeat_byte(1);
        let executor = Address::repeat_byte(2);
        let request = shield(TokenData::erc20(Address::repeat_byte(3)), 1_000);
        let mut adjusted = request.preimage.clone();
        adjusted.value = Uint::<120, 2>::from(998);
        let shield_event = Shield {
            treeNumber: U256::ONE,
            startPosition: U256::ZERO,
            commitments: vec![adjusted],
            shieldCiphertext: vec![request.ciphertext.clone()],
            fees: vec![U256::from(2)],
        };
        let nullifier = B256::repeat_byte(6);
        let commitment = B256::repeat_byte(7);
        let ciphertext = CommitmentCiphertext {
            ciphertext: [B256::ZERO; 4],
            blindedSenderViewingKey: B256::ZERO,
            blindedReceiverViewingKey: B256::ZERO,
            annotationData: Bytes::new(),
            memo: Bytes::new(),
        };
        let transactions = vec![Transaction {
            proof: SnarkProof::default(),
            merkleRoot: B256::ZERO,
            nullifiers: vec![nullifier],
            commitments: vec![commitment],
            boundParams: BoundParams::new_transact(
                1,
                0,
                1,
                vec![ciphertext.clone()],
                executor,
                B256::ZERO,
            ),
            unshieldPreimage: CommitmentPreimage::empty(),
        }];
        let call = RelayAdapt7702::executeCall {
            _transactions: transactions,
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: vec![Call {
                    to: executor,
                    value: U256::ZERO,
                    data: shieldCall {
                        _shieldRequests: vec![request],
                    }
                    .abi_encode()
                    .into(),
                }],
            },
            _nonce: U256::from(3),
            _signature: Bytes::new(),
        };
        let payload = issued(call.abi_encode());
        let private_logs = vec![
            event_log(
                &Nullified {
                    treeNumber: 1,
                    nullifier: vec![nullifier],
                },
                railgun,
            ),
            event_log(
                &Transact {
                    treeNumber: U256::ONE,
                    startPosition: U256::ZERO,
                    hash: vec![commitment],
                    ciphertext: vec![ciphertext],
                },
                railgun,
            ),
        ];
        assert_eq!(
            execution_effects(railgun, executor, &payload, &receipt(false, vec![])).unwrap(),
            ExecutorExecutionResult::Reverted
        );
        for logs in [vec![], private_logs.clone()] {
            assert_eq!(
                execution_effects(railgun, executor, &payload, &receipt(true, logs)).unwrap(),
                ExecutorExecutionResult::MissingEffects
            );
        }
        let mut complete = private_logs.clone();
        complete.push(event_log(&shield_event, railgun));
        assert_eq!(
            execution_effects(railgun, executor, &payload, &receipt(true, complete)).unwrap(),
            ExecutorExecutionResult::Executed
        );
        let mut wrong_amount = shield_event.clone();
        wrong_amount.commitments[0].value -= Uint::<120, 2>::from(1);
        let mut incomplete = private_logs.clone();
        incomplete.push(event_log(&wrong_amount, railgun));
        assert_eq!(
            execution_effects(railgun, executor, &payload, &receipt(true, incomplete)).unwrap(),
            ExecutorExecutionResult::MissingEffects
        );
        let mut wrong_contract = private_logs;
        wrong_contract.push(event_log(&shield_event, executor));
        assert_eq!(
            execution_effects(railgun, executor, &payload, &receipt(true, wrong_contract)).unwrap(),
            ExecutorExecutionResult::MissingEffects
        );
    }

    #[test]
    fn executor_multicall_recovery_tracks_the_selected_nft_without_private_fee_outputs() {
        let railgun = Address::repeat_byte(1);
        let executor = Address::repeat_byte(2);
        let request = shield(
            TokenData {
                tokenType: 1,
                tokenAddress: Address::repeat_byte(3),
                tokenSubID: U256::from(9),
            },
            1,
        );
        let mut event = Shield {
            treeNumber: U256::ONE,
            startPosition: U256::ZERO,
            commitments: vec![request.preimage.clone()],
            shieldCiphertext: vec![request.ciphertext.clone()],
            fees: vec![U256::ZERO],
        };
        let payload = issued(
            RelayAdapt7702::multicallCall {
                _requireSuccess: true,
                _calls: vec![Call {
                    to: executor,
                    value: U256::ZERO,
                    data: shieldCall {
                        _shieldRequests: vec![request],
                    }
                    .abi_encode()
                    .into(),
                }],
                _nonce: U256::from(3),
                _signature: Bytes::new(),
            }
            .abi_encode(),
        );
        assert_eq!(
            execution_effects(railgun, executor, &payload, &receipt(true, vec![])).unwrap(),
            ExecutorExecutionResult::MissingEffects
        );
        assert_eq!(
            execution_effects(
                railgun,
                executor,
                &payload,
                &receipt(true, vec![event_log(&event, railgun)])
            )
            .unwrap(),
            ExecutorExecutionResult::Executed
        );
        event.commitments[0].token.tokenSubID += U256::ONE;
        assert_eq!(
            execution_effects(
                railgun,
                executor,
                &payload,
                &receipt(true, vec![event_log(&event, railgun)])
            )
            .unwrap(),
            ExecutorExecutionResult::MissingEffects
        );
    }
}
