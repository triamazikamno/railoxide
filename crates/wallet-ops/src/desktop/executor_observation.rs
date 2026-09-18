use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::Duration;

use alloy::consensus::Transaction as _;
use alloy::network::TransactionResponse as _;
use alloy::network::primitives::{BlockTransactions, HeaderResponse as _};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::{SolCall, SolValue};
use broadcaster_core::contracts::railgun::{
    Call, Nullified, RelayAdapt7702, Shield, ShieldRequest, Transact, Transaction, shieldCall,
};
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};

use crate::HttpContext;
use crate::block_observer::fetch_checked_block_receipts;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion, ExecutorRecord,
    IssuedExecutorPayload,
};

const MAX_OBSERVATION_BLOCKS: u64 = 64;

mod recovery;

#[derive(Clone, Copy)]
enum NonceSource {
    Inspect,
    Signing(ExecutorNonceObservation),
    HistoryOnly,
}

pub(super) struct ExecutorHistoryObservation {
    pub(super) nonce: Option<ExecutorNonceObservation>,
    pub(super) block: alloy::eips::BlockNumHash,
    pub(super) inclusions: Vec<(B256, ExecutorPayloadInclusion)>,
    pub(super) recovery_inclusions: Vec<(B256, ExecutorPayloadInclusion)>,
}

/// Scan explicit blocks, never query a private transaction hash at a remote endpoint.
/// Signing supplies its already checked nonce and canonical block. Reuse that
/// snapshot within this operation; other explicit reconciliation supplies `None`.
pub(super) async fn observe_executor_history(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    record: &ExecutorRecord,
    range: Range<u64>,
    signing_nonce: Option<ExecutorNonceObservation>,
) -> Result<ExecutorHistoryObservation> {
    observe_history(
        chain,
        http,
        record,
        range,
        signing_nonce.map_or(NonceSource::Inspect, NonceSource::Signing),
    )
    .await
}

/// Private sync supplies a block location, not proof of executor execution.
/// Verify its receipt and expected effects without querying the account or hash.
pub(super) async fn observe_synced_executor_history(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    record: &ExecutorRecord,
    number: u64,
) -> Result<ExecutorHistoryObservation> {
    observe_history(
        chain,
        http,
        record,
        number..number.saturating_add(1),
        NonceSource::HistoryOnly,
    )
    .await
}

async fn observe_history(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
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
            inclusions: Vec::new(),
            recovery_inclusions: Vec::new(),
        });
    }
    let pool = QueryRpcPool::with_http_client(
        chain.rpc_route.endpoint_urls(),
        Duration::from_secs(30),
        http.rpc_client.clone(),
    );
    for provider in pool.available_providers() {
        if provider.provider.get_chain_id().await.ok() != Some(chain.chain_id) {
            continue;
        }
        if let Ok(observation) = observe_history_at_provider(
            &provider.provider,
            chain,
            record,
            range.clone(),
            nonce_source,
        )
        .await
        {
            return Ok(observation);
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
) -> Result<ExecutorHistoryObservation> {
    let address = record
        .address()
        .ok_or_else(|| eyre!("executor address is unavailable"))?;
    let latest = provider.get_block_number().await?;
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
    let confirmed = provider
        .get_block_by_number(confirmed_number.into())
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
    let nonce = match nonce_source {
        NonceSource::Signing(observed) => Some(observed.nonce()),
        NonceSource::Inspect => {
            super::executor_discovery::execution_nonce_at(provider, chain, address, block).await
        }
        NonceSource::HistoryOnly => None,
    };
    let railgun = chain.railgun_contract.parse()?;
    let mut numbers = range.clone().collect::<BTreeSet<_>>();
    // Revalidate previous inclusions, even outside this discovery page. An old
    // cached winner must not survive a reorg or an unavailable receipt read.
    for payload in record.issued() {
        if let Some(inclusion) = payload.inclusion()
            && inclusion.block().number <= confirmed_number
        {
            numbers.insert(inclusion.block().number);
        }
    }
    for transaction in record.recovery_transactions() {
        if let Some(inclusion) = transaction.inclusion()
            && inclusion.block().number <= confirmed_number
        {
            numbers.insert(inclusion.block().number);
        }
    }
    let mut inclusions = BTreeMap::new();
    let mut recovery_inclusions = Vec::new();
    for number in numbers {
        let observed = observe_block(provider, railgun, record, number).await?;
        recovery_inclusions.extend(observed.recovery);
        for (hash, inclusion) in observed.execution {
            let previous = inclusions.get(&hash).copied();
            if previous.is_none_or(|previous: ExecutorPayloadInclusion| {
                previous.result() != ExecutorExecutionResult::Executed
            }) {
                inclusions.insert(hash, inclusion);
            }
        }
    }
    let still_canonical = provider
        .get_block_by_number(confirmed_number.into())
        .await?
        .is_some_and(|current| current.header.num_hash() == block);
    if !still_canonical {
        return Err(eyre!("executor chain changed during observation"));
    }
    Ok(ExecutorHistoryObservation {
        nonce: nonce.map(|nonce| ExecutorNonceObservation::new(block, nonce)),
        block,
        inclusions: inclusions.into_iter().collect(),
        recovery_inclusions,
    })
}

#[derive(Default)]
struct ObservedExecutorBlock {
    execution: Vec<(B256, ExecutorPayloadInclusion)>,
    recovery: Vec<(B256, ExecutorPayloadInclusion)>,
}

async fn observe_block(
    provider: &DynProvider,
    railgun: Address,
    record: &ExecutorRecord,
    number: u64,
) -> Result<ObservedExecutorBlock> {
    let block = provider
        .get_block_by_number(number.into())
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
    receipt: &TransactionReceipt,
) -> Result<ExecutorExecutionResult> {
    if !receipt.status() {
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
    if !private_effects_present(railgun, &transactions, receipt)
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

fn private_effects_present(
    railgun: Address,
    transactions: &[Transaction],
    receipt: &TransactionReceipt,
) -> bool {
    let mut nullifiers = BTreeSet::new();
    let mut commitments = BTreeSet::new();
    for log in receipt
        .logs()
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
    receipt: &TransactionReceipt,
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
    use crate::vault::{ExecutorPayloadContext, ExecutorPayloadPurpose};
    use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{Bytes, Uint};
    use alloy::rpc::types::Log;
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
        .remove(&1)
        .unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(1, Vec::<url::Url>::new());
        let http = HttpContext::direct_for_tests();
        let record = empty_record(&chain);
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(50, B256::repeat_byte(50)), U256::ZERO);
        let history = observe_executor_history(&chain, &http, &record, 50..51, Some(observed))
            .await
            .unwrap();
        assert_eq!(history.nonce, Some(observed));
        assert!(history.inclusions.is_empty());
        assert!(history.recovery_inclusions.is_empty());
        // Neither an absent observation nor any previously issued payload can
        // use the offline branch, including ordinary recovery transactions.
        assert!(
            observe_executor_history(&chain, &http, &record, 50..51, None)
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
                observe_executor_history(&chain, &http, &previous, 50..51, Some(observed),)
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
        .remove(&1)
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
            // Signing already obtained the nonce at this exact block.
            responses.push_success(&block);
            responses.push_success(&block);
            let result = observe_history_at_provider(
                &provider,
                &chain,
                &record,
                50..51,
                NonceSource::Signing(ExecutorNonceObservation::new(pinned, U256::ZERO)),
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
        .remove(&1)
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
            )
            .await;
            if confirmed && !reorg {
                assert!(result.unwrap().nonce.is_none());
            } else {
                assert!(result.is_err());
            }
        }
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
