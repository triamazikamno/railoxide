//! Finish already-authorized sends independently of the selected wallet actor.
//!
//! Only prepared proofs are retained here; the encrypted actor records remain the
//! durable recovery source. No spending or viewing keys are held by these jobs.
use super::*;
use alloy::rpc::types::{Log, TransactionReceipt};
use alloy::sol_types::SolEvent;
use broadcaster_core::contracts::railgun::Transact;
use poi::poi::SingleCommitmentProofContext;

const RETRY_INTERVAL: Duration = Duration::from_secs(15);
const JOB_TIMEOUT: Duration = Duration::from_mins(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct SenderPoiOutbox {
    state: Mutex<OutboxState>,
}

#[derive(Default)]
struct OutboxState {
    generation: u64,
    closed: bool,
    jobs: HashMap<(u64, FixedBytes<32>), tokio::task::JoinHandle<()>>,
}

impl SenderPoiOutbox {
    fn spawn(
        &self,
        generation: u64,
        key: (u64, FixedBytes<32>),
        job: impl Future<Output = ()> + Send + 'static,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.generation != generation || state.jobs.contains_key(&key) {
            return;
        }
        state.jobs.insert(key, tokio::spawn(job));
    }

    pub(super) async fn cancel(&self, close: bool) {
        let jobs = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation = state.generation.wrapping_add(1);
            state.closed |= close;
            let jobs = std::mem::take(&mut state.jobs);
            for job in jobs.values() {
                job.abort();
            }
            jobs
        };
        for (_, job) in jobs {
            let _ = job.await;
        }
    }
}

impl Drop for SenderPoiOutbox {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in state.jobs.values() {
            job.abort();
        }
    }
}

pub(crate) struct SenderPoiSession {
    outbox: Arc<SenderPoiOutbox>,
    generation: u64,
    chain: ChainKey,
    finality_depth: u64,
    rpc: Arc<QueryRpcPool>,
    poi: PoiRpcClient,
    prepared: Mutex<BTreeMap<FixedBytes<32>, SingleCommitmentProofContext>>,
}

impl SenderPoiSession {
    pub(super) fn new(
        outbox: Arc<SenderPoiOutbox>,
        chain: ChainKey,
        finality_depth: u64,
        rpc: Arc<QueryRpcPool>,
        poi: PoiRpcClient,
    ) -> Self {
        let generation = outbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation;
        Self {
            outbox,
            generation,
            chain,
            finality_depth,
            rpc,
            poi,
            prepared: Mutex::default(),
        }
    }

    pub(crate) fn retain(&self, records: &[PendingOutputPoiContextRecord]) {
        let mut prepared = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for record in records {
            prepared.insert(
                record.output_commitment,
                SingleCommitmentProofContext {
                    txid_version: record.txid_version.clone(),
                    railgun_txid: record.railgun_txid,
                    utxo_tree_in: record.utxo_tree_in,
                    commitment: record.output_commitment,
                    npk: record.output_npk,
                    pre_transaction_pois_per_txid_leaf_per_list: record
                        .pre_transaction_pois_per_txid_leaf_per_list
                        .clone(),
                },
            );
        }
    }

    pub(super) fn submit(&self, tx_hash: FixedBytes<32>) {
        let contexts = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if contexts.is_empty() {
            return;
        }
        let chain = self.chain;
        let finality_depth = self.finality_depth;
        let rpc = Arc::clone(&self.rpc);
        let poi = self.poi.clone();
        self.outbox.spawn(self.generation, (chain.chain_id, tx_hash), async move {
            let result = tokio::time::timeout(JOB_TIMEOUT, submit_after_receipt(chain, tx_hash, &rpc, &poi, &contexts, finality_depth, RETRY_INTERVAL)).await;
            match result {
                Ok(Ok(count)) => tracing::info!(chain_id = chain.chain_id, outputs = count, "sender PPOI submission finished"),
                Ok(Err(error)) => tracing::warn!(chain_id = chain.chain_id, %error, "sender PPOI submission needs wallet recovery"),
                Err(_) => tracing::warn!(chain_id = chain.chain_id, "sender PPOI submission timed out; encrypted recovery records retained"),
            }
        });
    }
}

async fn submit_after_receipt(
    chain: ChainKey,
    tx_hash: FixedBytes<32>,
    rpc: &QueryRpcPool,
    poi: &PoiRpcClient,
    contexts: &BTreeMap<FixedBytes<32>, SingleCommitmentProofContext>,
    finality_depth: u64,
    retry_interval: Duration,
) -> Result<usize> {
    loop {
        let Some(provider) = rpc.random_provider() else {
            tokio::time::sleep(retry_interval).await;
            continue;
        };
        let receipt = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            provider.provider.get_transaction_receipt(tx_hash),
        )
        .await
        {
            Ok(Ok(Some(receipt))) => receipt,
            Ok(Ok(None)) => {
                tokio::time::sleep(retry_interval).await;
                continue;
            }
            _ => {
                rpc.mark_bad_provider(&provider);
                tokio::time::sleep(retry_interval).await;
                continue;
            }
        };
        if receipt.transaction_hash != tx_hash
            || receipt.block_hash.is_none()
            || receipt.block_number.is_none()
        {
            rpc.mark_bad_provider(&provider);
            continue;
        }
        if !receipt.status() {
            return Ok(0);
        }
        if finality_depth > 0 {
            match tokio::time::timeout(REQUEST_TIMEOUT, provider.provider.get_block_number()).await
            {
                Ok(Ok(head))
                    if receipt
                        .block_number
                        .is_some_and(|block| block <= head.saturating_sub(finality_depth)) => {}
                Ok(Ok(_)) => {
                    tokio::time::sleep(retry_interval).await;
                    continue;
                }
                _ => {
                    rpc.mark_bad_provider(&provider);
                    tokio::time::sleep(retry_interval).await;
                    continue;
                }
            }
        }
        let outputs = observed_outputs(chain.contract, &receipt, contexts)?;
        if outputs.is_empty() {
            return Err(eyre!("confirmed transaction contains no prepared outputs"));
        }
        let mut all_submitted = true;
        for (commitment, tree, position) in &outputs {
            let context = &contexts[commitment];
            match tokio::time::timeout(
                REQUEST_TIMEOUT,
                poi.submit_single_commitment_proofs(
                    &context.txid_version,
                    0,
                    chain.chain_id,
                    context,
                    *tree,
                    *position,
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    all_submitted = false;
                    tracing::warn!(chain_id = chain.chain_id, %error, "sender PPOI submission failed; retrying");
                }
                Err(_) => {
                    all_submitted = false;
                    tracing::warn!(
                        chain_id = chain.chain_id,
                        "sender PPOI request timed out; retrying"
                    );
                }
            }
        }
        if all_submitted {
            return Ok(outputs.len());
        }
        // Re-read the receipt on retry, including output positions after any reorg.
        tokio::time::sleep(retry_interval).await;
    }
}

fn observed_outputs(
    contract: Address,
    receipt: &TransactionReceipt,
    contexts: &BTreeMap<FixedBytes<32>, SingleCommitmentProofContext>,
) -> Result<Vec<(FixedBytes<32>, u64, u64)>> {
    let mut outputs = BTreeMap::new();
    for log in receipt.inner.logs() {
        if log.address() != contract || log.topic0() != Some(&Transact::SIGNATURE_HASH) {
            continue;
        }
        if log.removed
            || log.transaction_hash != Some(receipt.transaction_hash)
            || log.block_hash != receipt.block_hash
            || log.block_number != receipt.block_number
        {
            return Err(eyre!("inconsistent receipt event identity"));
        }
        for (commitment, tree, position) in transact_outputs(log)? {
            if contexts.contains_key(&commitment)
                && outputs.insert(commitment, (tree, position)).is_some()
            {
                return Err(eyre!("duplicate prepared output in receipt"));
            }
        }
    }
    Ok(outputs
        .into_iter()
        .map(|(commitment, (tree, position))| (commitment, tree, position))
        .collect())
}

fn transact_outputs(log: &Log) -> Result<Vec<(FixedBytes<32>, u64, u64)>> {
    let event = Transact::decode_log(&log.inner)?.data;
    if event.hash.len() != event.ciphertext.len() {
        return Err(eyre!("inconsistent Transact output count"));
    }
    let tree: u64 = event.treeNumber.try_into()?;
    let start: u64 = event.startPosition.try_into()?;
    event
        .hash
        .into_iter()
        .enumerate()
        .map(|(index, commitment)| {
            let position = start
                .checked_add(u64::try_from(index)?)
                .ok_or_else(|| eyre!("output position overflow"))?;
            Ok((commitment, tree, position))
        })
        .collect()
}
