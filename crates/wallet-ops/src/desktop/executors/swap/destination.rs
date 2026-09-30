//! A Bridge order's outcome on its destination chain, after its hand-off on this chain.
//!
//! Across's deposit record only locates the fill block. Delivery is verified from the
//! destination chain's finalized whole-block receipts, and only block identifiers reach that
//! chain's RPC. NEAR Intents delivery is 1Click's report and isn't verified on chain.
//!
//! An explicit status check of a refunding Across order also verifies the refund on this chain.
//! The refund transaction Across names is looked up only for its block number, and the refund
//! is matched in that block's finalized whole-block receipts. Like the explicit balance check
//! it accompanies, that lookup identifies the stealth account to this chain's RPC.

use alloy::eips::BlockNumHash;
use alloy::network::{
    AnyRpcBlock, AnyTransactionReceipt, ReceiptResponse, primitives::HeaderResponse as _,
};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, EthGetBlock, Provider as _};
use alloy::rpc::types::Log;
use broadcaster_core::contracts::across::{SpokePool, address_to_bytes32};
use broadcaster_core::contracts::cow::OrderUid;
use eyre::{Result, eyre};
use tracing::Instrument as _;

use super::bridge::SwapBridgeClients;
use super::settlement::Transfer;
use crate::ExecutorOwner;
use crate::block_observer::fetch_checked_block_receipts;
use crate::bridge::{
    AcrossClient, AcrossDepositStatus, BridgeApiError, NearIntentsClient, OneClickExecutionStatus,
};
use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::settings::{EffectiveChainConfig, resolve_effective_chain_rpc_route};
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, ExecutorOperationId, SwapBridgeOutcome,
    SwapDelivery, SwapObservation,
};

impl ExecutorOwner {
    /// One routine poll of a handed-off Bridge order's provider. Returns the order's outcome
    /// after the poll, persisting a new one. Orders without a hand-off, or with an outcome,
    /// are left alone: `NeedsAttention` waits for [`Self::check_swap_bridge`].
    /// `destination` is the delivery's destination chain.
    pub async fn observe_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        clients: &SwapBridgeClients,
        destination: &EffectiveChainConfig,
    ) -> Result<Option<SwapBridgeOutcome>> {
        self.track_swap_bridge(operation, uid, clients, destination, false)
            .await
    }

    /// A user-requested status check, which also asks again after `NeedsAttention`, and after an
    /// Across `Refunding`: a verified fill corrects it, and otherwise the refund Across names is
    /// verified on this chain.
    pub async fn check_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        clients: &SwapBridgeClients,
        destination: &EffectiveChainConfig,
    ) -> Result<Option<SwapBridgeOutcome>> {
        self.track_swap_bridge(operation, uid, clients, destination, true)
            .await
    }

    async fn track_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        clients: &SwapBridgeClients,
        destination: &EffectiveChainConfig,
        explicit: bool,
    ) -> Result<Option<SwapBridgeOutcome>> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let order = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        let observations = order.observations();
        let known = observations.bridge_outcome;
        let (SwapDelivery::Bridge(delivery), Some(terms), Some(handoff)) = (
            order.delivery(),
            order.bridge(),
            observations.bridge_handoff,
        ) else {
            return Ok(known);
        };
        // An explicit check asks Across again about a deposit it reported expired, until the
        // refund is verified on this chain.
        let rechecks_refund = explicit
            && known == Some(SwapBridgeOutcome::Refunding)
            && observations.bridge_refund.is_none()
            && matches!(terms, BridgeOrderTerms::Across(_));
        if known.is_some_and(|known| !explicit || known.is_final() && !rechecks_refund) {
            return Ok(known);
        }
        let (outcome, refund_tx) = match terms {
            BridgeOrderTerms::Across(across) => {
                let deposit_id = handoff
                    .deposit_id
                    .ok_or_else(|| eyre!("the Across deposit is unknown"))?;
                let fill = ExpectedFill {
                    origin_chain: self.chain.chain_id,
                    deposit_id,
                    executor: record
                        .address()
                        .ok_or_else(|| eyre!("swap account is unavailable"))?,
                    delivery,
                    terms: *across,
                };
                self.while_active(Box::pin(self.across_outcome(
                    &clients.across,
                    destination,
                    fill,
                )))
                .await?
            }
            BridgeOrderTerms::NearIntents(near) => (
                self.while_active(Box::pin(near_outcome(&clients.near, near.deposit_address)))
                    .await?,
                None,
            ),
        };
        tracing::debug!(
            target: "executor_observation",
            provider = ?terms.provider(),
            step = "swap_bridge",
            result = outcome_label(outcome),
            "finished"
        );
        // An inconclusive answer keeps the known outcome.
        let current = match outcome.filter(|outcome| Some(*outcome) != known) {
            Some(outcome) => {
                let _guard = self.lock_activity().await;
                self.require_record_unchanged(&record)?;
                self.store
                    .record_swap_bridge_outcome(operation, uid, outcome)?;
                self.notify_change();
                Some(outcome)
            }
            None => known,
        };
        // Only an explicit check looks the refund up, since the lookup names it to the RPC.
        if explicit
            && current == Some(SwapBridgeOutcome::Refunding)
            && let Some(refund_tx) = refund_tx
        {
            Box::pin(self.observe_swap_bridge_refund(operation, uid, refund_tx)).await?;
        }
        Ok(current)
    }

    /// Across's report locates a fill block, which is then checked on the destination chain.
    /// Expiry and refund are Across's word, returned with the refund transaction it names.
    async fn across_outcome(
        &self,
        across: &AcrossClient,
        destination: &EffectiveChainConfig,
        fill: ExpectedFill,
    ) -> Result<(Option<SwapBridgeOutcome>, Option<B256>)> {
        let Some(deposit) = trace_step(
            "swap_bridge_across_deposit",
            across.deposit(fill.origin_chain, fill.deposit_id),
        )
        .await?
        else {
            return Ok((None, None));
        };
        let number = match (deposit.status, deposit.fill_block_number) {
            (AcrossDepositStatus::Expired | AcrossDepositStatus::Refunded, _) => {
                return Ok((
                    Some(SwapBridgeOutcome::Refunding),
                    deposit.deposit_refund_tx,
                ));
            }
            (AcrossDepositStatus::Filled, Some(number)) => number,
            _ => return Ok((None, None)),
        };
        resolve_effective_chain_rpc_route(fill.delivery.destination_chain, destination)?;
        // Only the destination chain's own SpokePool emits fills that deliver the deposit.
        let spoke_pool = destination
            .bridge_profile()
            .ok_or_else(|| eyre!("the destination network doesn't support bridging"))?
            .spoke_pool();
        let endpoints = ObservationEndpoints::new(destination, &self.http);
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_bridge_fill",
                Box::pin(read_fill(
                    &endpoint.provider,
                    destination.finality_depth,
                    number,
                    spoke_pool,
                    &fill,
                )),
            )
            .instrument(span)
            .await;
            match result {
                Ok(outcome) => {
                    endpoints.succeeded(&endpoint);
                    return Ok((outcome, None));
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Delivery verification on the destination network is unavailable. The swap will be checked again."
        ))
    }

    /// Verify the refund of a refunding Across order's deposit on this chain, located by the
    /// refund transaction Across names, and persist its block.
    async fn observe_swap_bridge_refund(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        refund_tx: B256,
    ) -> Result<()> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let order = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        let observed = order.observations();
        let (Some(BridgeOrderTerms::Across(terms)), Some(executor)) =
            (order.bridge(), record.address())
        else {
            return Ok(());
        };
        if observed.bridge_outcome != Some(SwapBridgeOutcome::Refunding)
            || observed.bridge_refund.is_some()
        {
            return Ok(());
        }
        let refund = ExpectedRefund {
            spoke_pool: terms.spoke_pool,
            token: terms.input_token,
            executor,
            amount: terms.input_amount,
        };
        for endpoint in self.endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_bridge_refund",
                self.while_active(Box::pin(read_refund(
                    &endpoint.provider,
                    self.chain.finality_depth,
                    refund_tx,
                    &refund,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(found) => {
                    self.endpoints.succeeded(&endpoint);
                    let Some(found) = found else {
                        return Ok(());
                    };
                    let _guard = self.lock_activity().await;
                    self.require_record_unchanged(&record)?;
                    self.store
                        .record_swap_bridge_refund(operation, uid, found)?;
                    self.notify_change();
                    return Ok(());
                }
                Err(error) => self.endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Refund verification is unavailable. Check status again later."
        ))
    }
}

/// 1Click's report. Unknown deposit addresses and unfinished states leave no outcome.
async fn near_outcome(
    near: &NearIntentsClient,
    deposit_address: Address,
) -> Result<Option<SwapBridgeOutcome>> {
    let status = match trace_step("swap_bridge_near_status", near.status(deposit_address)).await {
        Ok(status) => status,
        Err(BridgeApiError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(match status.status {
        OneClickExecutionStatus::Success => Some(SwapBridgeOutcome::DeliveredReported {
            amount_out: status.amount_out,
            transaction_hash: status
                .destination_tx_hashes
                .iter()
                .find_map(|hash| hash.parse().ok()),
        }),
        OneClickExecutionStatus::Refunded => Some(SwapBridgeOutcome::Refunding),
        OneClickExecutionStatus::Failed | OneClickExecutionStatus::IncompleteDeposit => {
            Some(SwapBridgeOutcome::NeedsAttention)
        }
        OneClickExecutionStatus::KnownDepositTx
        | OneClickExecutionStatus::PendingDeposit
        | OneClickExecutionStatus::Processing
        | OneClickExecutionStatus::Unknown => None,
    })
}

/// The fill that delivers an Across deposit.
struct ExpectedFill {
    origin_chain: u64,
    deposit_id: U256,
    executor: Address,
    delivery: BridgeDelivery,
    terms: AcrossOrderTerms,
}

impl ExpectedFill {
    /// The event keeps the signed output amount. The executed one can be higher: a slow fill
    /// pays the deposit less the LP fee.
    fn matches(&self, fill: &SpokePool::FilledRelay) -> bool {
        let recipient = address_to_bytes32(self.delivery.receiver);
        fill.originChainId == U256::from(self.origin_chain)
            && fill.depositId == self.deposit_id
            && fill.depositor == address_to_bytes32(self.executor)
            && fill.recipient == recipient
            && fill.inputToken == address_to_bytes32(self.terms.input_token)
            && fill.inputAmount == self.terms.input_amount
            && fill.outputToken == address_to_bytes32(self.terms.output_token)
            && fill.outputAmount == self.terms.output_amount
            // A relayer may fill with a depositor-signed update; only the signed receiver, and
            // at least the signed amount, deliver.
            && fill.relayExecutionInfo.updatedRecipient == recipient
            && fill.relayExecutionInfo.updatedOutputAmount >= self.terms.output_amount
    }
}

/// The transfer that refunds an Across deposit to the executor on this chain.
struct ExpectedRefund {
    spoke_pool: Address,
    token: Address,
    executor: Address,
    amount: U256,
}

impl ExpectedRefund {
    /// Across may refund several deposits to one address in one transfer, so the value only has
    /// to cover this deposit.
    fn matches(&self, log: &Log) -> bool {
        log.address() == self.token
            && log.log_decode::<Transfer>().is_ok_and(|transfer| {
                let transfer = transfer.inner.data;
                transfer.from == self.spoke_pool
                    && transfer.to == self.executor
                    && transfer.value >= self.amount
            })
    }
}

/// Match `fill` in the finalized receipts of block `number`, sending only block identifiers.
/// A block that isn't final yet, has no matching fill, or left the canonical chain during the
/// read leaves no outcome. The outcome carries the fill's executed amount.
async fn read_fill(
    provider: &DynProvider,
    finality_depth: u64,
    number: u64,
    spoke_pool: Address,
    fill: &ExpectedFill,
) -> Result<Option<SwapBridgeOutcome>> {
    let Some((identity, receipts)) = finalized_receipts(provider, finality_depth, number).await?
    else {
        return Ok(None);
    };
    let mut found = None;
    for receipt in receipts {
        for log in receipt.inner.logs() {
            if log.address() != spoke_pool {
                continue;
            }
            let Ok(event) = log.log_decode::<SpokePool::FilledRelay>() else {
                continue;
            };
            let event = event.inner.data;
            if !fill.matches(&event) {
                continue;
            }
            if found.is_some() {
                return Err(eyre!("fill block contains ambiguous fill evidence"));
            }
            found = Some(SwapBridgeOutcome::DeliveredVerified {
                block: identity,
                transaction_hash: receipt.transaction_hash(),
                output_amount: event.relayExecutionInfo.updatedOutputAmount,
            });
        }
    }
    if found.is_none() {
        return Ok(None);
    }
    let canonical = still_canonical(provider, identity).await?;
    Ok(found.filter(|_| canonical))
}

/// Match `refund` in the finalized receipts of the block that includes `refund_tx`. Only that
/// block's number is taken from the transaction's receipt. A refund that isn't included or final
/// yet, a block without the matching transfer, or one that left the canonical chain during the
/// read leaves none.
async fn read_refund(
    provider: &DynProvider,
    finality_depth: u64,
    refund_tx: B256,
    refund: &ExpectedRefund,
) -> Result<Option<SwapObservation>> {
    // Across names no refund block, so the refund's receipt is read for its block number alone.
    let located = trace_step("swap_bridge_refund_locate", async {
        provider
            .client()
            .request::<_, Option<AnyTransactionReceipt>>("eth_getTransactionReceipt", (refund_tx,))
            .await
    })
    .await?;
    let Some(number) = located.and_then(|receipt| receipt.block_number()) else {
        return Ok(None);
    };
    let Some((identity, receipts)) = finalized_receipts(provider, finality_depth, number).await?
    else {
        return Ok(None);
    };
    let found = receipts
        .iter()
        .find(|receipt| receipt.inner.logs().iter().any(|log| refund.matches(log)))
        .map(|receipt| SwapObservation {
            block: identity,
            transaction_hash: Some(receipt.transaction_hash()),
        });
    if found.is_none() {
        return Ok(None);
    }
    let canonical = still_canonical(provider, identity).await?;
    Ok(found.filter(|_| canonical))
}

/// The successful receipts of block `number` once it is final, read whole by block identifiers
/// only. `None` while the block isn't final yet.
async fn finalized_receipts(
    provider: &DynProvider,
    finality_depth: u64,
    number: u64,
) -> Result<Option<(BlockNumHash, Vec<AnyTransactionReceipt>)>> {
    let head = trace_step("swap_bridge_block_head", provider.get_block_number()).await?;
    if head
        .checked_sub(finality_depth)
        .is_none_or(|safe| number > safe)
    {
        return Ok(None);
    }
    let block = trace_step("swap_bridge_block", async {
        EthGetBlock::<AnyRpcBlock>::by_number(number.into(), provider.client()).await
    })
    .await?
    .ok_or_else(|| eyre!("bridge evidence block is unavailable"))?;
    let identity = block.header.num_hash();
    if identity.number != number {
        return Err(eyre!("bridge evidence block has the wrong number"));
    }
    let hashes = block.transactions.hashes().collect::<Vec<_>>();
    let receipts = trace_step(
        "swap_bridge_block_receipts",
        fetch_checked_block_receipts(provider, identity, &hashes),
    )
    .await
    .map_err(|_| eyre!("whole-block bridge receipts are incomplete or unavailable"))?;
    let receipts = receipts
        .into_iter()
        .filter(ReceiptResponse::status)
        .collect::<Vec<_>>();
    // The enclosing receipt supplies inclusion; inconsistent log metadata is not evidence.
    if receipts.iter().any(|receipt| {
        receipt.inner.logs().iter().any(|log| {
            log.removed
                || log.block_hash != Some(identity.hash)
                || log.block_number != Some(number)
                || log.transaction_hash != Some(receipt.transaction_hash())
        })
    }) {
        return Err(eyre!("bridge receipt logs have inconsistent inclusion"));
    }
    Ok(Some((identity, receipts)))
}

/// Whether `identity` is still canonical. A block fetched before a reorg may still be served.
async fn still_canonical(provider: &DynProvider, identity: BlockNumHash) -> Result<bool> {
    Ok(trace_step("swap_bridge_block_canonical_recheck", async {
        provider.get_block_by_number(identity.number.into()).await
    })
    .await?
    .is_some_and(|block| block.header.num_hash() == identity))
}

/// The outcome's kind, without its amounts or hashes.
const fn outcome_label(outcome: Option<SwapBridgeOutcome>) -> &'static str {
    match outcome {
        None => "pending",
        Some(SwapBridgeOutcome::DeliveredVerified { .. }) => "delivered_verified",
        Some(SwapBridgeOutcome::DeliveredReported { .. }) => "delivered_reported",
        Some(SwapBridgeOutcome::Refunding) => "refunding",
        Some(SwapBridgeOutcome::NeedsAttention) => "needs_attention",
    }
}
