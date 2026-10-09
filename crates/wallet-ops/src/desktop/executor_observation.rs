use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use tracing::Instrument as _;

use crate::HttpContext;
use crate::settings::EffectiveChainConfig;
use crate::vault::ExecutorNonceObservation;

/// How long an endpoint whose request failed sits out executor observations.
const ENDPOINT_COOLDOWN: Duration = Duration::from_mins(3);
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
pub(super) struct ObservationEndpoints {
    route: crate::RpcChainRoute,
    client: reqwest::Client,
    /// Completes once the first admission returned, whether or not the pool was kept.
    admitted: tokio::sync::OnceCell<()>,
    /// Never locked across an await.
    pool: Mutex<PoolSlot>,
    /// Pool index of the last endpoint that served an observation, or `usize::MAX`.
    preferred: AtomicUsize,
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
        }
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
    /// other than a revert. A chain change doesn't count.
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

/// An account's code and execution nonce at one confirmed canonical block.
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

async fn read_account_at_provider(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    requested: Option<u64>,
) -> Result<ExecutorAccountRead> {
    let block = confirmed_block_at_provider(provider, chain, requested).await?;
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
mod tests {
    use super::*;
    use alloy::eips::BlockNumHash;
    use alloy::network::primitives::BlockTransactions;
    use alloy::primitives::{B256, U256};

    #[tokio::test]
    async fn account_read_returns_code_and_nonce_at_a_confirmed_block_only() {
        use alloy::providers::{ProviderBuilder, mock::Asserter};
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let address = Address::repeat_byte(1);
        let delegate = chain.accepted_executor_profile().unwrap().delegate();
        let designator = alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
        let code = Bytes::from([&designator[..], delegate.as_slice()].concat());
        let confirmed_tip = 50;
        // `None` reads the confirmed tip, an earlier block is read as requested, and
        // a block past the confirmed tip is refused.
        for requested in [None, Some(40), Some(confirmed_tip + 1)] {
            let responses = Asserter::new();
            let provider = ProviderBuilder::new()
                .connect_mocked_client(responses.clone())
                .erased();
            let number = requested.unwrap_or(confirmed_tip);
            let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
            block.header.inner.number = number;
            block.header.hash = B256::repeat_byte(9);
            block.transactions = BlockTransactions::Full(Vec::new());
            responses.push_success(&format!("0x{:x}", confirmed_tip + chain.finality_depth));
            responses.push_success(&block);
            responses.push_success(&code);
            responses.push_success(&Bytes::from(U256::from(7).to_be_bytes::<32>()));
            let result = read_account_at_provider(&provider, &chain, address, requested).await;
            if number > confirmed_tip {
                assert!(result.is_err());
                continue;
            }
            let read = result.unwrap();
            let at = BlockNumHash::new(number, B256::repeat_byte(9));
            assert_eq!(read.block, at);
            assert_eq!(read.code, code);
            assert_eq!(
                read.nonce,
                Some(ExecutorNonceObservation::new(at, U256::from(7)))
            );
        }
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
        for _ in 0..2 {
            let read = read_executor_account(&endpoints, &chain, Address::repeat_byte(1), None)
                .await
                .unwrap();
            assert_eq!(read.block, BlockNumHash::new(50, B256::repeat_byte(50)));
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
}
