use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alloy::network::primitives::HeaderResponse as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::Log;
use alloy::sol_types::SolCall;
use alloy::transports::{RpcError, TransportError};
use broadcaster_core::contracts::railgun::{
    Call, Nullified, ShieldRequest, Transact, Transaction, shieldCall,
};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool, RpcAdmission};
use eyre::{Result, eyre};
use poi::SensitiveUrl;
use tracing::Instrument as _;

use crate::rpc_broker::EndpointHealthOutcome;
use crate::settings::EffectiveChainConfig;
use crate::vault::ExecutorNonceObservation;
use crate::{HttpContext, OperationHttpClient};

/// How long in all one observation waits in [`ObservationEndpoints::later_providers`]: just
/// above the RPC identity check's timeout, by which every endpoint's first check finished.
/// Tests wait less.
pub(super) const LATE_ADMISSION_WAIT: Duration =
    Duration::from_secs(if cfg!(test) { 2 } else { 11 });
/// How often a wait for a later admission looks at the pool.
const LATE_ADMISSION_POLL: Duration = Duration::from_millis(100);

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

/// The chain endpoints one executor read observes through, over its operation's client.
///
/// Admission is the wallet's RPC identity admission: a route that requires identity
/// checks verifies each endpoint's chain once, and a failed check is retried in the
/// background with backoff while the pool lives, not on every observation. Endpoint
/// health is the wallet-wide RPC broker's: each attempt's outcome is reported to it, and
/// endpoints it has withdrawn are left out, in its order, unless no other admitted one is
/// eligible.
///
/// The pool lives until this is dropped. Admission's background retries hold only a weak
/// reference to it, so dropping this lets them stop.
pub(super) struct ObservationEndpoints {
    route: crate::ClassRoute,
    clients: crate::rpc_broker::RpcClients,
    broker: Arc<crate::RpcBroker>,
    /// Completes once the first admission returned, whether or not the pool was kept.
    admitted: tokio::sync::OnceCell<()>,
    /// Never locked across an await.
    pool: Mutex<Option<Arc<QueryRpcPool>>>,
}

impl ObservationEndpoints {
    /// Observes through `chain`'s endpoints that accept `class`, on `client`'s route.
    pub(super) fn new(
        chain: &EffectiveChainConfig,
        http: &HttpContext,
        class: crate::RpcTrafficClass,
        client: &OperationHttpClient,
    ) -> Result<Self, crate::RpcClassUnavailable> {
        Ok(Self {
            route: chain.rpc_route.for_class(class)?.isolated(client.clone()),
            clients: http.rpc_clients(),
            broker: http.rpc_broker(),
            admitted: tokio::sync::OnceCell::new(),
            pool: Mutex::new(None),
        })
    }

    /// The endpoints of this class, admitted or not. Decisions that count endpoint answers
    /// count against this.
    pub(super) fn configured_count(&self) -> usize {
        self.route.endpoints().len()
    }

    fn current_pool(&self) -> Option<Arc<QueryRpcPool>> {
        self.pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The broker's eligible endpoints for this route, in its order. `None` when the lookup
    /// fails; selection then keeps the pool's order rather than failing the read.
    async fn candidates(&self) -> Option<Vec<SensitiveUrl>> {
        self.broker
            .endpoint_candidates(self.route.route(), None)
            .await
            .ok()
    }

    /// Admitted endpoints the broker reports eligible, in its order. When none of them is
    /// admitted, every admitted endpoint, so withdrawals can't stop every observation.
    pub(super) async fn providers(&self) -> Vec<ProviderHandle> {
        let started = Instant::now();
        tracing::debug!(
            target: "executor_observation",
            step = "endpoint_admission",
            initialized = self.admitted.initialized(),
            "started"
        );
        self.admitted
            .get_or_init(|| async {
                // Nothing marks this pool's endpoints bad; the broker keeps their health.
                let pool = Arc::new(
                    self.route
                        .query_rpc_pool(&self.clients, Duration::ZERO)
                        .with_pending_admission(),
                );
                // Returns at the first admitted endpoint or once every first check
                // failed. The pool is kept either way and admits endpoints as they
                // pass.
                let _ = self.route.admit_sync_pool(&self.clients, &pool, None).await;
                *self.pool.lock().unwrap_or_else(PoisonError::into_inner) = Some(pool);
            })
            .await;
        let Some(pool) = self.current_pool() else {
            return Vec::new();
        };
        let candidates = self.candidates().await;
        let available = pool.available_providers();
        tracing::debug!(
            target: "executor_observation",
            step = "endpoint_admission",
            elapsed_ms = started.elapsed().as_millis(),
            available = available.len(),
            "finished"
        );
        let Some(candidates) = candidates else {
            return available;
        };
        let eligible = in_candidate_order(available.clone(), &candidates);
        if eligible.is_empty() {
            available
        } else {
            eligible
        }
    }

    /// Endpoints admitted since an earlier [`Self::providers`] call: the available ones the
    /// broker reports eligible whose pool index isn't in `tried`, as soon as there is one,
    /// waiting until `deadline` while such an endpoint's admission is pending. Empty at the
    /// deadline, and once no such admission is pending.
    pub(super) async fn later_providers(
        &self,
        tried: &BTreeSet<usize>,
        deadline: tokio::time::Instant,
    ) -> Vec<ProviderHandle> {
        // One lookup for the whole wait; withdrawals during it apply to the next read.
        let candidates = self.candidates().await;
        let eligible = |url: &SensitiveUrl| {
            candidates
                .as_ref()
                .is_none_or(|candidates| candidates.contains(url))
        };
        loop {
            // The pool is looked up on every pass and not held while waiting.
            let pending = {
                let Some(pool) = self.current_pool() else {
                    return Vec::new();
                };
                // Read before the providers: an endpoint admitted between the two reads then
                // still counts as pending, and the next pass finds it.
                let pending = self
                    .route
                    .endpoints()
                    .iter()
                    .enumerate()
                    .any(|(index, url)| {
                        !tried.contains(&index)
                            && eligible(url)
                            && pool.provider_admission(index) == Some(RpcAdmission::Pending)
                    });
                let mut providers = pool.available_providers();
                providers.retain(|provider| !tried.contains(&provider.index));
                let providers = match &candidates {
                    Some(candidates) => in_candidate_order(providers, candidates),
                    None => providers,
                };
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

    /// Each of these endpoints once, for a caller that tries one after another.
    pub(super) const fn attempts(&self) -> EndpointAttempts<'_> {
        EndpointAttempts {
            endpoints: self,
            tried: BTreeSet::new(),
            queued: None,
            deadline: None,
        }
    }

    pub(super) fn succeeded(&self, provider: &ProviderHandle) {
        self.report(provider, EndpointHealthOutcome::Healthy);
    }

    pub(super) fn report(&self, provider: &ProviderHandle, outcome: EndpointHealthOutcome) {
        self.broker
            .report_endpoint_outcome(self.route.chain_id(), &provider.url, outcome);
    }

    /// Reports the failure to the broker, classified as the broker classifies its own requests.
    /// A chain change is neutral.
    pub(super) fn failed(&self, provider: &ProviderHandle, error: &eyre::Report) {
        self.failed_after(provider, error, EndpointHealthOutcome::Healthy);
    }

    /// [`Self::failed`] for an attempt whose earlier requests had `earlier` as their outcome,
    /// reported together with the failure as one.
    pub(super) fn failed_after(
        &self,
        provider: &ProviderHandle,
        error: &eyre::Report,
        earlier: EndpointHealthOutcome,
    ) {
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
        self.report(
            provider,
            earlier.combine(EndpointHealthOutcome::for_direct_failure(error)),
        );
    }
}

/// The endpoints of [`ObservationEndpoints::providers`], then those admitted later, each at
/// most once. While one that wasn't tried is still being admitted, the later admissions are
/// waited for, [`LATE_ADMISSION_WAIT`] in all.
pub(super) struct EndpointAttempts<'a> {
    endpoints: &'a ObservationEndpoints,
    /// By pool index.
    tried: BTreeSet<usize>,
    /// `None` before the first endpoint is asked for.
    queued: Option<std::vec::IntoIter<ProviderHandle>>,
    deadline: Option<tokio::time::Instant>,
}

impl EndpointAttempts<'_> {
    /// The next endpoint to try, or `None` once there is none.
    pub(super) async fn next(&mut self) -> Option<ProviderHandle> {
        if self.queued.is_none() {
            self.queued = Some(self.endpoints.providers().await.into_iter());
        }
        loop {
            if let Some(queued) = &mut self.queued {
                for provider in queued.by_ref() {
                    if self.tried.insert(provider.index) {
                        return Some(provider);
                    }
                }
            }
            if self.tried.len() >= self.endpoints.configured_count() {
                return None;
            }
            let until = *self
                .deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + LATE_ADMISSION_WAIT);
            let later = self.endpoints.later_providers(&self.tried, until).await;
            if later.is_empty() {
                return None;
            }
            self.queued = Some(later.into_iter());
        }
    }
}

/// `providers` whose URL is among `candidates`, in candidate order. Pool indices are per
/// pool, so they are matched by URL.
fn in_candidate_order(
    providers: Vec<ProviderHandle>,
    candidates: &[SensitiveUrl],
) -> Vec<ProviderHandle> {
    let mut ranked = providers
        .into_iter()
        .filter_map(|provider| {
            let rank = candidates
                .iter()
                .position(|candidate| *candidate.expose_url() == provider.url)?;
            Some((rank, provider))
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, provider)| provider).collect()
}

/// An account's code and execution nonce at one canonical block: a confirmed one, or the
/// latest for [`read_executor_account_at_latest`].
pub(super) struct ExecutorAccountRead {
    pub(super) block: alloy::eips::BlockNumHash,
    pub(super) code: Bytes,
    /// `None` when the code is not a layout the execution nonce is read under.
    pub(super) nonce: Option<ExecutorNonceObservation>,
}

/// Read an account's state by address at `requested`, or at the confirmed tip. No
/// block contents, receipts or transaction hashes are involved.
pub(super) async fn read_executor_account(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    address: Address,
    requested: Option<u64>,
) -> Result<ExecutorAccountRead> {
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match trace_step(
            "account_rpc",
            read_account_at_provider(&provider.provider, chain, address, requested),
        )
        .instrument(span)
        .await
        {
            Ok(read) => {
                endpoints.succeeded(&provider);
                return Ok(read);
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!("executor account state is unavailable"))
}

/// Read an account's state by address at the latest block, which a reorg can still remove.
/// It goes through the same endpoints as [`read_executor_account`]. Nothing may apply it to a
/// record: only a confirmed read resolves a nonce.
pub(super) async fn read_executor_account_at_latest(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    address: Address,
) -> Result<ExecutorAccountRead> {
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        let read = trace_step("latest_account_rpc", async {
            let block = trace_step("latest_account_block", async {
                provider
                    .provider
                    .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                    .await
            })
            .await?
            .ok_or_else(|| eyre!("latest executor block is unavailable"))?
            .header
            .num_hash();
            read_account_at_block(&provider.provider, chain, address, block).await
        })
        .instrument(span)
        .await;
        match read {
            Ok(read) => {
                endpoints.succeeded(&provider);
                return Ok(read);
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!("executor account state is unavailable"))
}

async fn read_account_at_provider(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    requested: Option<u64>,
) -> Result<ExecutorAccountRead> {
    let block = confirmed_block_at_provider(provider, chain, requested).await?;
    read_account_at_block(provider, chain, address, block).await
}

async fn read_account_at_block(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block: alloy::eips::BlockNumHash,
) -> Result<ExecutorAccountRead> {
    // Both reads name the block by hash, so a reorg after the header read fails
    // them and cannot mix two blocks.
    let block_id = alloy::eips::BlockId::hash_canonical(block.hash);
    let code = trace_step("account_code", async {
        provider.get_code_at(address).block_id(block_id).await
    })
    .await?;
    let started = Instant::now();
    tracing::debug!(target: "executor_observation", step = "account_nonce", "started");
    let nonce = super::executor_discovery::execution_nonce_with_code(
        provider,
        chain,
        address,
        block_id,
        Some(&code),
        false,
    )
    .await;
    tracing::debug!(
        target: "executor_observation",
        step = "account_nonce",
        elapsed_ms = started.elapsed().as_millis(),
        available = nonce.is_some(),
        "finished"
    );
    // The helper reports a failed read like an unreadable layout. Under a layout it
    // reads, a missing nonce is the endpoint's failure.
    if nonce.is_none()
        && chain.accepted_executor_profile().is_some_and(|profile| {
            code.is_empty()
                || super::executor_discovery::matches_executor_delegation(&code, profile)
        })
    {
        return Err(eyre!("executor execution nonce is unavailable"));
    }
    Ok(ExecutorAccountRead {
        block,
        code,
        nonce: nonce.map(|nonce| ExecutorNonceObservation::new(block, nonce)),
    })
}

async fn confirmed_block_at_provider(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    requested: Option<u64>,
) -> Result<alloy::eips::BlockNumHash> {
    let latest = trace_step("account_head", async { provider.get_block_number().await }).await?;
    let confirmed_tip = latest.saturating_sub(chain.finality_depth);
    let number = requested.unwrap_or(confirmed_tip);
    if number > confirmed_tip {
        return Err(eyre!(
            "requested executor account block has not reached the configured confirmation depth"
        ));
    }
    let block = trace_step("account_block", async {
        provider.get_block_by_number(number.into()).await
    })
    .await?
    .ok_or_else(|| eyre!("confirmed executor block is unavailable"))?
    .header
    .num_hash();
    if block.number != number {
        return Err(eyre!(
            "executor observation block does not match its requested height"
        ));
    }
    Ok(block)
}

/// Read one explicitly requested asset balance at the confirmed canonical tip.
pub(super) async fn read_executor_asset_balance(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    address: Address,
    asset: crate::ExecutorAsset,
) -> Result<(U256, alloy::eips::BlockNumHash)> {
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        let read = trace_step("asset_balance_rpc", async {
            let block = confirmed_block_at_provider(&provider.provider, chain, None).await?;
            let balance = super::executor_discovery::asset_balance(
                &provider.provider,
                address,
                asset,
                alloy::eips::BlockId::hash_canonical(block.hash),
            )
            .await
            .ok_or_else(|| eyre!("executor asset balance is unavailable"))?;
            Ok::<_, eyre::Report>((balance, block))
        })
        .instrument(span)
        .await;
        match read {
            Ok(read) => {
                endpoints.succeeded(&provider);
                return Ok(read);
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!("executor asset balance is unavailable"))
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

#[cfg(test)]
mod tests;
