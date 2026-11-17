use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use alloy::consensus::BlockHeader as _;
use alloy::eips::BlockNumHash;
use alloy::network::primitives::{BlockTransactions, HeaderResponse as _};
use alloy::network::{AnyRpcBlock, AnyTransactionReceipt};
use alloy::network::{BlockResponse as _, ReceiptResponse as _, TransactionResponse as _};
use alloy::primitives::{Address, B256, FixedBytes};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::Log;
use alloy::transports::{RpcError, TransportError};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use eyre::{Result, WrapErr as _, eyre};

use crate::public_wallet::{PublicTransactionFamily, PublicTransactionTrackingContext};
use crate::rpc_broker::EndpointHealthOutcome;
use crate::{ClassRoute, RpcBroker, TxReceiptOutput};

const MAX_BLOCKS_PER_POLL: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockIdentity {
    number: u64,
    hash: B256,
    parent_hash: B256,
    timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockObservation {
    pub(crate) receipt: Option<(usize, TxReceiptOutput)>,
    pub(crate) head: Option<(usize, u64)>,
}

/// Failures carry the poll's outcome for broker health.
#[derive(Debug)]
enum ProviderPollFailure {
    Lagging,
    UnsupportedReceipts(EndpointHealthOutcome),
    Failed(EndpointHealthOutcome),
}

#[derive(Debug)]
enum BlockFetch {
    Missing,
    /// A failed request or an invalid block, with its outcome for broker health.
    Invalid(EndpointHealthOutcome),
    Valid(BlockIdentity, Vec<B256>),
}

#[derive(Debug)]
enum ReceiptFetch {
    Unsupported,
    Failed(EndpointHealthOutcome),
    Invalid,
    Valid(TxReceiptOutput),
}

pub(crate) struct BlockObserver {
    query_rpc_pool: Arc<QueryRpcPool>,
    finality_depth: u64,
    next_block: u64,
    registered: HashMap<B256, usize>,
    history: VecDeque<BlockIdentity>,
    unsupported_receipts: HashSet<usize>,
    active_provider: Option<usize>,
    rpc_broker: Arc<RpcBroker>,
    chain_id: u64,
    tracking: Option<PublicTransactionFamily>,
}

impl BlockObserver {
    /// `query_rpc_pool` must be built from `route`. The baseline starts on the first admitted
    /// endpoint the broker reports eligible, or in pool order if the broker can't answer.
    pub(crate) async fn establish(
        query_rpc_pool: Arc<QueryRpcPool>,
        finality_depth: u64,
        rpc_broker: Arc<RpcBroker>,
        route: &ClassRoute,
    ) -> Result<Self> {
        let providers = query_rpc_pool.available_providers();
        let active_provider = rpc_broker
            .endpoint_candidates(route.route(), None)
            .await
            .ok()
            .and_then(|eligible| {
                eligible.iter().find_map(|endpoint| {
                    providers
                        .iter()
                        .find(|provider| &provider.url == endpoint.expose_url())
                        .map(|provider| provider.index)
                })
            });
        let mut observer = Self {
            query_rpc_pool,
            finality_depth,
            next_block: 0,
            registered: HashMap::new(),
            history: VecDeque::new(),
            unsupported_receipts: HashSet::new(),
            active_provider,
            rpc_broker,
            chain_id: route.chain_id(),
            tracking: None,
        };
        observer.establish_baseline().await?;
        Ok(observer)
    }

    fn report(&self, provider: &ProviderHandle, outcome: EndpointHealthOutcome) {
        self.rpc_broker
            .report_endpoint_outcome(self.chain_id, &provider.url, outcome);
    }

    /// Attach captured ownership after establishing the pre-handoff baseline.
    pub(crate) fn with_tracking(mut self, context: &PublicTransactionTrackingContext) -> Self {
        let tracking = context.start_family();
        for hash in self.registered.keys() {
            tracking.register(*hash);
        }
        self.tracking = Some(tracking);
        self
    }

    pub(crate) fn register(&mut self, tx_hash: FixedBytes<32>, attempt_id: usize) {
        if let Some(tracking) = &self.tracking {
            tracking.register(tx_hash);
        }
        self.registered.insert(tx_hash, attempt_id);
    }

    pub(crate) fn needs_observation(&self) -> bool {
        !self.registered.is_empty()
            && self
                .tracking
                .as_ref()
                .is_some_and(|tracking| !tracking.is_included())
    }

    pub(crate) fn discovery_exhausted(&self) {
        if let Some(tracking) = &self.tracking {
            tracking.discovery_exhausted();
        }
    }

    pub(crate) async fn poll(&mut self) -> Result<BlockObservation> {
        let result = self.poll_inner().await;
        if result.is_err()
            && let Some(tracking) = &self.tracking
        {
            tracking.unavailable();
        }
        result
    }

    async fn poll_inner(&mut self) -> Result<BlockObservation> {
        if self.registered.is_empty() {
            return Ok(BlockObservation {
                receipt: None,
                head: None,
            });
        }

        let mut providers = self.query_rpc_pool.available_providers();
        if let Some(active_provider) = self.active_provider {
            providers.sort_by_key(|provider| usize::from(provider.index != active_provider));
        }
        if providers.is_empty() {
            return Err(eyre!("privacy-preserving block observation is unavailable"));
        }

        let mut lagging = false;
        let mut saw_unsupported_receipts = false;
        let mut saw_failed_provider = false;
        for provider in providers {
            let result = self.poll_provider(&provider).await;
            // A lagging endpoint answered every request.
            let outcome = match &result {
                Ok(_) | Err(ProviderPollFailure::Lagging) => EndpointHealthOutcome::Healthy,
                Err(
                    ProviderPollFailure::UnsupportedReceipts(outcome)
                    | ProviderPollFailure::Failed(outcome),
                ) => *outcome,
            };
            self.report(&provider, outcome);
            match result {
                Ok(observation) => {
                    self.active_provider = Some(provider.index);
                    if observation.receipt.is_none()
                        && observation.head.is_some()
                        && let Some(tracking) = &self.tracking
                    {
                        tracking.recovered();
                    }
                    // Inclusion invalidates rather than notifies: an equal head does not evict,
                    // and this endpoint may lead the one that served a cached latest read.
                    if observation.receipt.is_some() {
                        self.rpc_broker.invalidate_block(self.chain_id);
                    } else if let Some((_, head)) = observation.head {
                        self.rpc_broker.notify_block(self.chain_id, head);
                    }
                    return Ok(observation);
                }
                Err(ProviderPollFailure::Lagging) => lagging = true,
                Err(ProviderPollFailure::UnsupportedReceipts(_)) => {
                    saw_unsupported_receipts = true;
                    self.unsupported_receipts.insert(provider.index);
                }
                Err(ProviderPollFailure::Failed(_)) => {
                    saw_failed_provider = true;
                    self.query_rpc_pool.mark_bad_provider(&provider);
                    let rpc = crate::http::redact_url_for_display(&provider.url);
                    tracing::warn!(%rpc, "block-scoped query RPC observation failed");
                }
            }
        }

        if (saw_unsupported_receipts || saw_failed_provider)
            && let Some(tracking) = &self.tracking
        {
            tracking.unavailable();
        }
        if lagging {
            return Ok(BlockObservation {
                receipt: None,
                head: None,
            });
        }
        if saw_unsupported_receipts {
            return Err(eyre!(
                "privacy-preserving block receipt observation is unavailable"
            ));
        }
        Err(eyre!("privacy-preserving block observation is unavailable"))
    }

    async fn establish_baseline(&mut self) -> Result<()> {
        let mut providers = self.query_rpc_pool.available_providers();
        if let Some(active_provider) = self.active_provider {
            providers.sort_by_key(|provider| usize::from(provider.index != active_provider));
        }
        if providers.is_empty() {
            return Err(eyre!("privacy-preserving block observation is unavailable"));
        }
        for provider in providers {
            let head = provider.provider.get_block_number().await;
            self.report(
                &provider,
                head.as_ref()
                    .map_or_else(EndpointHealthOutcome::for_direct_error, |_| {
                        EndpointHealthOutcome::Healthy
                    }),
            );
            let Ok(head) = head else {
                self.query_rpc_pool.mark_bad_provider(&provider);
                let rpc = crate::http::redact_url_for_display(&provider.url);
                tracing::warn!(%rpc, "block observation baseline failed");
                continue;
            };
            self.next_block = head.saturating_add(1);
            self.active_provider = Some(provider.index);
            return Ok(());
        }
        Err(eyre!(
            "privacy-preserving block observation baseline is unavailable"
        ))
    }

    async fn poll_provider(
        &mut self,
        provider: &ProviderHandle,
    ) -> std::result::Result<BlockObservation, ProviderPollFailure> {
        let head = provider
            .provider
            .get_block_number()
            .await
            .map_err(|error| {
                ProviderPollFailure::Failed(EndpointHealthOutcome::for_direct_error(&error))
            })?;

        // Valid responses that fail the observer's own checks are neutral.
        if let Some(tip) = self.history.back().copied() {
            match fetch_block(provider, tip.number).await {
                BlockFetch::Missing if head < tip.number => {
                    return Err(ProviderPollFailure::Lagging);
                }
                BlockFetch::Valid(identity, _) if identity == tip => {}
                BlockFetch::Valid(_, _) => {
                    self.rollback_to_common_ancestor(provider).await?;
                }
                BlockFetch::Missing => {
                    return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral));
                }
                BlockFetch::Invalid(outcome) => return Err(ProviderPollFailure::Failed(outcome)),
            }
        }

        if head < self.next_block {
            return Err(ProviderPollFailure::Lagging);
        }

        let end = head.min(
            self.next_block
                .saturating_add(MAX_BLOCKS_PER_POLL.saturating_sub(1)),
        );
        for number in self.next_block..=end {
            let (identity, transactions) = match fetch_block(provider, number).await {
                BlockFetch::Valid(identity, transactions) => (identity, transactions),
                BlockFetch::Missing => {
                    return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral));
                }
                BlockFetch::Invalid(outcome) => return Err(ProviderPollFailure::Failed(outcome)),
            };
            if identity.number != number {
                return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Strike));
            }
            if self
                .history
                .back()
                .is_some_and(|previous| identity.parent_hash != previous.hash)
            {
                self.rollback_to_common_ancestor(provider).await?;
                return Ok(BlockObservation {
                    receipt: None,
                    head: self.processed_head(provider.index),
                });
            }

            let matching_hashes = transactions
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, hash)| self.registered.contains_key(hash))
                .collect::<Vec<_>>();
            if matching_hashes.len() > 1 {
                return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral));
            }
            if let Some((transaction_index, tx_hash)) = matching_hashes.first().copied() {
                // Skipped locally, after this poll's requests succeeded.
                if self.unsupported_receipts.contains(&provider.index) {
                    return Err(ProviderPollFailure::UnsupportedReceipts(
                        EndpointHealthOutcome::Healthy,
                    ));
                }
                return match fetch_receipt(provider, identity, &transactions, tx_hash).await {
                    ReceiptFetch::Valid(receipt) => {
                        if let Some(tracking) = &self.tracking {
                            tracking.included(
                                tx_hash,
                                BlockNumHash::new(identity.number, identity.hash),
                                transaction_index as u64,
                            );
                        }
                        crate::desktop::observe_activity_inclusion(
                            tx_hash,
                            identity.number,
                            identity.timestamp,
                            receipt.status,
                        );
                        self.record(identity);
                        self.next_block = number.saturating_add(1);
                        Ok(BlockObservation {
                            receipt: Some((self.registered[&tx_hash], receipt)),
                            head: self.processed_head(provider.index),
                        })
                    }
                    // The broker strikes -32601, as for any endpoint-blamed JSON-RPC code.
                    ReceiptFetch::Unsupported => Err(ProviderPollFailure::UnsupportedReceipts(
                        EndpointHealthOutcome::Strike,
                    )),
                    ReceiptFetch::Failed(outcome) => Err(ProviderPollFailure::Failed(outcome)),
                    ReceiptFetch::Invalid => {
                        Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Strike))
                    }
                };
            }

            self.record(identity);
            self.next_block = number.saturating_add(1);
        }

        Ok(BlockObservation {
            receipt: None,
            head: self.processed_head(provider.index),
        })
    }

    async fn rollback_to_common_ancestor(
        &mut self,
        provider: &ProviderHandle,
    ) -> std::result::Result<(), ProviderPollFailure> {
        if let Some(tracking) = &self.tracking {
            tracking.pending();
        }
        // A reorg beyond retained history is the observer's limit, not the endpoint's failure.
        if self.history.len() < 2 {
            return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral));
        }
        for index in (0..self.history.len() - 1).rev() {
            let expected = self.history[index];
            match fetch_block(provider, expected.number).await {
                BlockFetch::Valid(identity, _) if identity == expected => {
                    self.history.truncate(index + 1);
                    self.next_block = expected.number.saturating_add(1);
                    crate::desktop::observe_activity_rollback(
                        self.registered.keys(),
                        expected.number,
                    );
                    return Ok(());
                }
                BlockFetch::Missing => {
                    return Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral));
                }
                BlockFetch::Invalid(outcome) => return Err(ProviderPollFailure::Failed(outcome)),
                BlockFetch::Valid(_, _) => {}
            }
        }
        Err(ProviderPollFailure::Failed(EndpointHealthOutcome::Neutral))
    }

    fn record(&mut self, identity: BlockIdentity) {
        self.history.push_back(identity);
        let capacity = usize::try_from(self.finality_depth)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
            .max(1);
        while self.history.len() > capacity {
            self.history.pop_front();
        }
    }

    fn processed_head(&self, provider_index: usize) -> Option<(usize, u64)> {
        self.history
            .back()
            .map(|identity| (provider_index, identity.number))
    }
}

/// A transaction's sender and receipt logs, matched locally within its whole block.
#[derive(Debug)]
pub(crate) struct SourceTransaction {
    pub(crate) from: Address,
    pub(crate) logs: Vec<Log>,
}

pub(crate) async fn resolve_source_transaction_by_block(
    query_rpc_pool: &QueryRpcPool,
    block_number: u64,
    tx_hash: FixedBytes<32>,
) -> Result<SourceTransaction> {
    let providers = query_rpc_pool.available_providers();
    let mut last_error = None;
    for provider in providers {
        match fetch_source_transaction(&provider, block_number, tx_hash).await {
            Ok(source) => return Ok(source),
            Err(error) => {
                query_rpc_pool.mark_bad_provider(&provider);
                let rpc = crate::http::redact_url_for_display(&provider.url);
                tracing::warn!(%rpc, error = %format_args!("{error:#}"), "source block transaction resolution failed");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no RPC endpoints are available")))
        .wrap_err("privacy-preserving source transaction resolution is unavailable")
}

/// Errors contain only local descriptions and numeric RPC/HTTP codes. The caller tries the
/// next provider and never falls back to an exact-hash request.
async fn fetch_source_transaction(
    provider: &ProviderHandle,
    block_number: u64,
    tx_hash: FixedBytes<32>,
) -> Result<SourceTransaction> {
    // A full block may contain chain-specific envelopes unrelated to the target transaction.
    let block = provider
        .provider
        .client()
        .request::<_, Option<AnyRpcBlock>>(
            "eth_getBlockByNumber",
            (alloy::eips::BlockNumberOrTag::Number(block_number), true),
        )
        .await
        .map_err(|error| eyre!("eth_getBlockByNumber: {}", rpc_failure_reason(&error)))?
        .ok_or_else(|| eyre!("eth_getBlockByNumber returned no block"))?;
    if block.header().number() != block_number {
        return Err(eyre!(
            "eth_getBlockByNumber returned the wrong block number"
        ));
    }

    let mut sender = None;
    let mut matches = 0;
    let BlockTransactions::Full(transactions) = block.transactions() else {
        return Err(eyre!(
            "eth_getBlockByNumber omitted full transaction bodies"
        ));
    };
    let mut hashes = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        hashes.push(transaction.tx_hash());
        if transaction.tx_hash() == tx_hash {
            matches += 1;
            sender = Some(transaction.from());
        }
    }
    if matches != 1 {
        return Err(eyre!(
            "source transaction matched {matches} entries in the block; expected one"
        ));
    }
    let from = sender.ok_or_else(|| eyre!("source transaction sender is unavailable"))?;

    let receipts = fetch_checked_block_receipts(
        &provider.provider,
        BlockNumHash::new(block_number, block.header().hash()),
        &hashes,
    )
    .await
    .wrap_err("eth_getBlockReceipts")?;
    let receipt = receipts
        .into_iter()
        .find(|receipt| receipt.transaction_hash() == tx_hash)
        .ok_or_else(|| eyre!("eth_getBlockReceipts omitted the source transaction receipt"))?;
    Ok(SourceTransaction {
        from,
        logs: receipt.inner.logs().to_vec(),
    })
}

async fn fetch_block(provider: &ProviderHandle, number: u64) -> BlockFetch {
    let block = match provider.provider.get_block_by_number(number.into()).await {
        Ok(Some(block)) => block,
        Ok(None) => return BlockFetch::Missing,
        Err(error) => return BlockFetch::Invalid(EndpointHealthOutcome::for_direct_error(&error)),
    };
    if block.header().number() != number {
        return BlockFetch::Invalid(EndpointHealthOutcome::Strike);
    }
    let BlockTransactions::Hashes(transactions) = block.transactions() else {
        return BlockFetch::Invalid(EndpointHealthOutcome::Strike);
    };
    BlockFetch::Valid(
        BlockIdentity {
            number,
            hash: block.header().hash(),
            parent_hash: block.header().parent_hash(),
            timestamp: block.header().timestamp(),
        },
        transactions.clone(),
    )
}

async fn fetch_receipt(
    provider: &ProviderHandle,
    block: BlockIdentity,
    transactions: &[B256],
    tx_hash: B256,
) -> ReceiptFetch {
    let receipts = match fetch_checked_block_receipts(
        &provider.provider,
        BlockNumHash::new(block.number, block.hash),
        transactions,
    )
    .await
    {
        Ok(receipts) => receipts,
        Err(BlockReceiptsError::Unsupported) => return ReceiptFetch::Unsupported,
        Err(BlockReceiptsError::Failed(_, outcome)) => return ReceiptFetch::Failed(outcome),
        Err(BlockReceiptsError::Invalid) => return ReceiptFetch::Invalid,
    };
    let Some(receipt) = receipts
        .into_iter()
        .find(|receipt| receipt.transaction_hash() == tx_hash)
    else {
        return ReceiptFetch::Invalid;
    };
    let status = receipt.status();
    let gas_used = receipt.gas_used();
    let contract_address = receipt
        .contract_address()
        .map(|address| address.to_checksum(None));
    ReceiptFetch::Valid(TxReceiptOutput {
        tx_hash: tx_hash.to_string(),
        status,
        block_number: block.number,
        gas_used,
        contract_address,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BlockReceiptsError {
    #[error("method is unsupported (RPC -32601)")]
    Unsupported,
    /// A redacted reason and the request's outcome for broker health.
    #[error("{0}")]
    Failed(String, EndpointHealthOutcome),
    #[error("receipts are missing, incomplete, or inconsistent with the block")]
    Invalid,
}

/// RPC error messages and response bodies may contain credentials or wallet identifiers.
/// Preserve the failure category and status code without formatting those opaque strings.
fn rpc_failure_reason(error: &TransportError) -> String {
    match error {
        RpcError::ErrorResp(payload) => format!("RPC error {}", payload.code),
        RpcError::Transport(error) => error.as_http_error().map_or_else(
            || "transport failure".to_owned(),
            |error| format!("HTTP {}", error.status),
        ),
        RpcError::DeserError { .. } => "response could not be decoded".to_owned(),
        RpcError::NullResp => "no RPC response".to_owned(),
        _ => "RPC client failure".to_owned(),
    }
}

/// Read a whole block's receipts and match identities locally. Executor outcome
/// observation uses the same completeness checks as ordinary transaction observation.
pub(crate) async fn fetch_checked_block_receipts(
    provider: &DynProvider,
    block: BlockNumHash,
    transactions: &[B256],
) -> std::result::Result<Vec<AnyTransactionReceipt>, BlockReceiptsError> {
    // Whole-block responses include chain-specific receipts, even when the
    // transaction we observe uses a standard Ethereum envelope.
    let result = provider
        .client()
        .request::<_, Option<Vec<AnyTransactionReceipt>>>(
            "eth_getBlockReceipts",
            (alloy::eips::BlockId::from(block.hash),),
        )
        .await;
    let receipts = match result {
        Ok(Some(receipts)) => receipts,
        Ok(None) => return Err(BlockReceiptsError::Invalid),
        Err(error)
            if error
                .as_error_resp()
                .is_some_and(|response| response.code == -32601) =>
        {
            return Err(BlockReceiptsError::Unsupported);
        }
        Err(error) => {
            return Err(BlockReceiptsError::Failed(
                rpc_failure_reason(&error),
                EndpointHealthOutcome::for_direct_error(&error),
            ));
        }
    };
    let expected = transactions.iter().copied().collect::<HashSet<_>>();
    if expected.len() != transactions.len() || receipts.len() != transactions.len() {
        return Err(BlockReceiptsError::Invalid);
    }
    let mut seen = HashSet::with_capacity(receipts.len());
    for receipt in &receipts {
        let receipt_tx_hash = receipt.transaction_hash();
        if !seen.insert(receipt_tx_hash)
            || !expected.contains(&receipt_tx_hash)
            || receipt.block_hash() != Some(block.hash)
            || receipt.block_number() != Some(block.number)
        {
            return Err(BlockReceiptsError::Invalid);
        }
    }
    if seen != expected {
        return Err(BlockReceiptsError::Invalid);
    }
    Ok(receipts)
}

#[cfg(test)]
mod tests;
