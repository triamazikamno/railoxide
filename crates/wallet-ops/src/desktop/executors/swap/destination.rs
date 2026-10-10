//! A Bridge order's outcome on its destination chain, after its hand-off on this chain.
//!
//! Across's deposit record only locates the fill block. Delivery is verified from the
//! destination chain's finalized whole-block receipts, and only block identifiers reach that
//! chain's RPC. A fill counts when the destination chain's pinned `SpokePool` emitted it, or,
//! for a public delivery, when its receipt also holds the output token's transfer of at least
//! the signed amount to the receiver. Anyone can emit a fill event, so only the pinned pool's
//! is trusted for the executed amount, and a fill that counts by its transfer delivers that
//! transfer's value. A private delivery's fill pays Across's handler, and the same receipt then
//! shows whether the destination stealth account received the token and shielded it.
//!
//! A public delivery is Across's report, not verified on chain, when the final fill block holds
//! no such fill or no endpoint of the destination chain serves whole-block receipts. Routine
//! polling stops there. An explicit status check asks Across again, and replaces the report
//! with a verified fill or with the refund Across then reports. NEAR Intents delivery is
//! 1Click's report and isn't verified on chain.
//!
//! An explicit status check of a refunding Across order also verifies the refund on this chain.
//! The refund transaction Across names is looked up only for its block number, and the refund
//! is matched in that block's finalized whole-block receipts. Like the explicit balance check
//! it accompanies, that lookup identifies the stealth account to this chain's RPC.
//!
//! A swap paid from a Public account is tracked by the same code from the other side: its
//! record is on the destination chain's owner, which reads the fill on its own chain and the
//! refund, paid to the Public account, on the chain that account pays on. [`BridgeTracking`]
//! is what the two share: the expected fill, the recorded outcome, the chain the fill is read
//! on and where a new outcome is written.

use std::collections::BTreeSet;

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
use crate::block_observer::{BlockReceiptsError, fetch_checked_block_receipts};
use crate::bridge::{
    AcrossClient, AcrossDepositStatus, BridgeApiError, NearIntentsClient, OneClickExecutionStatus,
};
use crate::desktop::executor_observation::{LATE_ADMISSION_WAIT, ObservationEndpoints, trace_step};
use crate::settings::{EffectiveChainConfig, resolve_effective_chain_rpc_route};
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgePrivateDelivery, BridgeProvider,
    BridgeSurplus, ExecutorOperationId, ExecutorRecord, SwapBridgeOutcome, SwapDelivery,
    SwapObservation, SwapUseId, SwapUseRecord, SwapUseRole,
};

impl ExecutorOwner {
    /// One routine poll of a handed-off Bridge order's provider. Returns the order's outcome
    /// after the poll, persisting a new one. Orders without a hand-off, or with an outcome,
    /// are left alone: `NeedsAttention` and a delivery Across only reported wait for
    /// [`Self::check_swap_bridge`].
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
    /// verified on this chain. It asks again after an Across `DeliveredReported` too, which a
    /// verified fill or a refund Across now reports replaces.
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
        if !asks_again(
            known,
            explicit,
            matches!(terms, BridgeOrderTerms::Across(_)),
            observations.bridge_refund.is_some(),
        ) {
            return Ok(known);
        }
        let target = OutcomeTarget::Order(uid);
        let (current, refund_tx) = match terms {
            BridgeOrderTerms::Across(across) => {
                let deposit_id = handoff
                    .deposit_id
                    .ok_or_else(|| eyre!("the Across deposit is unknown"))?;
                let fill = ExpectedFill {
                    origin_chain: self.chain.chain_id,
                    deposit_id,
                    depositor: record
                        .address()
                        .ok_or_else(|| eyre!("swap account is unavailable"))?,
                    delivery,
                    terms: *across,
                };
                Box::pin(self.track_across(
                    &clients.across,
                    BridgeTracking {
                        record: &record,
                        target,
                        fill,
                        known,
                        destination,
                        endpoints: None,
                    },
                ))
                .await?
            }
            BridgeOrderTerms::NearIntents(near) => {
                let outcome = self
                    .while_active(Box::pin(near_outcome(&clients.near, near.deposit_address)))
                    .await?;
                let current = self
                    .record_bridge_outcome(&record, target, terms.provider(), known, outcome)
                    .await?;
                (current, None)
            }
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

    /// One poll of Across about the deposit `tracking` describes. Returns the outcome after
    /// the poll, persisting a new one, and the refund transaction Across names.
    async fn track_across(
        &self,
        across: &AcrossClient,
        tracking: BridgeTracking<'_>,
    ) -> Result<(Option<SwapBridgeOutcome>, Option<B256>)> {
        let BridgeTracking {
            record,
            target,
            fill,
            known,
            destination,
            endpoints,
        } = tracking;
        let (outcome, refund_tx) = self
            .while_active(Box::pin(self.across_outcome(
                across,
                destination,
                endpoints,
                fill,
            )))
            .await?;
        // A reported fill never replaces a known outcome: a refund yields only to a
        // verified fill, and an earlier report stands whatever Across now reports.
        let reported = matches!(outcome, Some(SwapBridgeOutcome::DeliveredReported { .. }));
        let outcome = outcome.filter(|_| known.is_none() || !reported);
        let current = self
            .record_bridge_outcome(record, target, BridgeProvider::Across, known, outcome)
            .await?;
        Ok((current, refund_tx))
    }

    /// Persist `outcome`, what `provider` answered about the deposit `target` names in
    /// `record`, and return the outcome after it. An inconclusive answer keeps the known one.
    async fn record_bridge_outcome(
        &self,
        record: &ExecutorRecord,
        target: OutcomeTarget,
        provider: BridgeProvider,
        known: Option<SwapBridgeOutcome>,
        outcome: Option<SwapBridgeOutcome>,
    ) -> Result<Option<SwapBridgeOutcome>> {
        tracing::debug!(
            target: "executor_observation",
            provider = ?provider,
            step = "swap_bridge",
            result = outcome_label(outcome),
            "finished"
        );
        let Some(outcome) = outcome.filter(|outcome| Some(*outcome) != known) else {
            return Ok(known);
        };
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(record)?;
        let operation = record.operation();
        match target {
            OutcomeTarget::Order(uid) => {
                self.store
                    .record_swap_bridge_outcome(operation, uid, outcome)?;
            }
            OutcomeTarget::PublicSwap(swap_use) => {
                self.store
                    .record_public_swap_bridge_outcome(operation, swap_use, outcome)?;
            }
        }
        self.notify_change();
        Ok(Some(outcome))
    }

    /// Across's report locates a fill block, which is then checked on the destination chain.
    /// Expiry and refund are Across's word, returned with the refund transaction it names.
    /// The fill is read through `endpoints`, or through endpoints opened for `destination`
    /// once there is a fill block to read.
    ///
    /// A public delivery is `DeliveredReported`, with the amount and fill transaction Across
    /// names, in two cases: an endpoint read the final fill block whole and it holds no
    /// candidate fill, or every endpoint of the destination chain answered that whole-block
    /// receipts are unsupported. Any other failed read is an error, so the swap is checked
    /// again, and so is an endpoint that wasn't admitted in time to answer. A private delivery
    /// is never reported.
    async fn across_outcome(
        &self,
        across: &AcrossClient,
        destination: &EffectiveChainConfig,
        endpoints: Option<&ObservationEndpoints>,
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
        // A fill emitted by the SpokePool pinned for the destination chain delivers the
        // deposit. A chain without one relies on the token transfer in the fill's receipt.
        let spoke_pool = destination
            .bridge_destination()
            .ok_or_else(|| eyre!("the destination network doesn't support bridging"))?
            .spoke_pool();
        // Only a private delivery's fill is followed by a shield on the destination chain, sent
        // by the handler of that chain's bridge profile.
        let shield = if fill.delivery.is_private() {
            Some(ExpectedShield {
                handler: destination
                    .bridge_profile()
                    .ok_or_else(|| eyre!("the destination network doesn't support bridging"))?
                    .multicall_handler(),
                railgun: destination.require_railgun()?.deployment.contract,
            })
        } else {
            None
        };
        // Across's word for the delivery. A private delivery's balance shows through sync, so it
        // is never taken.
        let reported =
            (!fill.delivery.is_private()).then_some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(deposit.output_amount),
                transaction_hash: deposit.fill_tx,
            });
        let opened;
        let endpoints = if let Some(endpoints) = endpoints {
            endpoints
        } else {
            opened = ObservationEndpoints::new(destination, &self.http);
            &opened
        };
        let read = Box::pin(read_fill_from_endpoints(
            endpoints,
            destination,
            number,
            spoke_pool,
            &fill,
            shield,
        ))
        .await;
        let unsupported = match read {
            Ok(read) => {
                let outcome = match read {
                    FillRead::Found(outcome) => Some(outcome),
                    FillRead::NoCandidate => reported,
                    FillRead::Unresolved => None,
                };
                return Ok((outcome, None));
            }
            Err(unsupported) => unsupported,
        };
        // Endpoints that weren't admitted or sit out a cool-down didn't answer, so the count is
        // taken against the whole route. One endpoint failing another way keeps the swap
        // unresolved: a timeout must not become a final label.
        if let Some(reported) = reported
            && unsupported > 0
            && unsupported == destination.rpc_route.endpoints().len()
        {
            return Ok((Some(reported), None));
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
            payee: executor,
            amount: terms.input_amount,
        };
        let Some(found) = self
            .read_refund_from_endpoints(
                &self.endpoints,
                self.chain.finality_depth,
                refund_tx,
                &refund,
            )
            .await?
        else {
            return Ok(());
        };
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(&record)?;
        self.store
            .record_swap_bridge_refund(operation, uid, found)?;
        self.notify_change();
        Ok(())
    }

    /// [`read_refund`] through `endpoints`, those of the chain the deposit was made on, from
    /// the first that serves it. `None` when that endpoint doesn't hold the refund yet.
    async fn read_refund_from_endpoints(
        &self,
        endpoints: &ObservationEndpoints,
        finality_depth: u64,
        refund_tx: B256,
        refund: &ExpectedRefund,
    ) -> Result<Option<SwapObservation>> {
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_bridge_refund",
                self.while_active(Box::pin(read_refund(
                    &endpoint.provider,
                    finality_depth,
                    refund_tx,
                    refund,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(found) => {
                    endpoints.succeeded(&endpoint);
                    return Ok(found);
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Refund verification is unavailable. Check status again later."
        ))
    }

    /// Track the Across deposit of a Public-paid swap on this chain, as `observe_swap_bridge`
    /// does for a stealth pair. This chain is the delivery's destination, so the fill is read
    /// through this owner's own endpoints. `None` while there is no hand-off yet.
    pub async fn observe_public_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        across: &AcrossClient,
    ) -> Result<Option<SwapBridgeOutcome>> {
        self.track_public_swap_bridge(operation, swap_use, across, false)
            .await
    }

    /// The explicit status check, which may replace an outcome as `check_swap_bridge` may: a
    /// verified fill corrects a `Refunding` whose refund isn't verified. The refund itself is
    /// verified by [`Self::observe_public_swap_refund`], on the chain the Public account pays
    /// on.
    pub async fn check_public_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        across: &AcrossClient,
    ) -> Result<Option<SwapBridgeOutcome>> {
        self.track_public_swap_bridge(operation, swap_use, across, true)
            .await
    }

    async fn track_public_swap_bridge(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        across: &AcrossClient,
        explicit: bool,
    ) -> Result<Option<SwapBridgeOutcome>> {
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            swap,
            ..
        }) = record.swap_use(swap_use).map(SwapUseRecord::role)
        else {
            return Err(eyre!(
                "this stealth account isn't claimed by a swap paid from a Public account"
            ));
        };
        let observations = swap.observations();
        let known = observations.bridge_outcome;
        let (Some(terms), Some(handoff), Some(deposited)) = (
            swap.bridge(),
            observations.bridge_handoff,
            observations.deposited,
        ) else {
            return Ok(known);
        };
        if !asks_again(known, explicit, true, observations.bridge_refund.is_some()) {
            return Ok(known);
        }
        let fill = ExpectedFill {
            origin_chain: *origin_chain,
            deposit_id: handoff
                .deposit_id
                .ok_or_else(|| eyre!("the Across deposit is unknown"))?,
            depositor: *source,
            // The delivery the swap signed: private, to this account.
            delivery: BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: self.chain.chain_id,
                receiver: record
                    .address()
                    .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?,
                destination_token: *destination_token,
                surplus: BridgeSurplus::Reshield,
                private: Some(BridgePrivateDelivery {
                    on_shield_failure: swap.approval().on_shield_failure,
                }),
            },
            // The fill repeats the deposit's event, whose amounts an order's hook sets above
            // the signed minimums.
            terms: AcrossOrderTerms {
                input_amount: deposited.input_amount,
                output_amount: deposited.output_amount,
                ..*terms
            },
        };
        let (current, _) = Box::pin(self.track_across(
            across,
            BridgeTracking {
                record: &record,
                target: OutcomeTarget::PublicSwap(swap_use),
                fill,
                known,
                destination: &self.chain,
                endpoints: Some(&self.endpoints),
            },
        ))
        .await?;
        Ok(current)
    }

    /// Verify Across's refund of a refunding Public-paid swap to its Public account, in the
    /// origin chain's finalized receipts, and record it. The refund pays the deposited input
    /// amount of the deposit's input token from `origin`'s pinned `SpokePool` to the Public
    /// account. Across is asked for the refund transaction, which is looked up on `origin` for
    /// its block number alone. That chain already sees the Public account.
    ///
    /// Returns `Some` exactly when this call newly verified the refund, which is when the
    /// Public account's balances on `origin` are due a refresh. A swap that isn't refunding,
    /// a refund that is already recorded and one that isn't final yet all return `None`.
    /// Nothing is recovered: the tokens are back in the Public account. The destination
    /// stealth account's shield stays signed, and keeps guarding that account.
    pub async fn observe_public_swap_refund(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        across: &AcrossClient,
    ) -> Result<Option<SwapObservation>> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let observed = claimed.swap.observations();
        if observed.bridge_outcome != Some(SwapBridgeOutcome::Refunding)
            || observed.bridge_refund.is_some()
        {
            return Ok(None);
        }
        let (Some(terms), Some(deposited), Some(deposit_id)) = (
            claimed.swap.bridge(),
            observed.deposited,
            observed
                .bridge_handoff
                .and_then(|handoff| handoff.deposit_id),
        ) else {
            return Ok(None);
        };
        let deposit = self
            .while_active(async {
                Ok(trace_step(
                    "swap_bridge_across_deposit",
                    across.deposit(origin.chain_id, deposit_id),
                )
                .await?)
            })
            .await?;
        let Some(refund_tx) = deposit.and_then(|deposit| deposit.deposit_refund_tx) else {
            return Ok(None);
        };
        let refund = ExpectedRefund {
            spoke_pool: claimed.spoke_pool,
            token: terms.input_token,
            payee: claimed.source,
            amount: deposited.input_amount,
        };
        let endpoints = ObservationEndpoints::new(origin, &self.http);
        let Some(found) = self
            .read_refund_from_endpoints(&endpoints, origin.finality_depth, refund_tx, &refund)
            .await?
        else {
            return Ok(None);
        };
        let _guard = self.lock_activity().await;
        let current = self.swap_account_record(operation)?.and_then(|record| {
            record
                .public_swap_use(swap_use)
                .map(|(_, swap)| swap.observations())
        });
        // Another observation of the swap wrote meanwhile. This read is repeated by the next
        // call rather than written over it.
        if current != Some(observed) {
            return Ok(None);
        }
        self.store
            .record_public_swap_bridge_refund(operation, swap_use, found)?;
        self.notify_change();
        Ok(Some(found))
    }
}

/// Whether a poll asks the provider about a deposit whose recorded outcome is `known`. A
/// routine poll asks only while there is none. An explicit check asks again after
/// `NeedsAttention`, after an Across `Refunding` until its refund is verified, and after an
/// Across `DeliveredReported`, which nothing on the destination chain backs.
const fn asks_again(
    known: Option<SwapBridgeOutcome>,
    explicit: bool,
    across: bool,
    refund_verified: bool,
) -> bool {
    let Some(known) = known else {
        return true;
    };
    let rechecks_refund =
        matches!(known, SwapBridgeOutcome::Refunding) && !refund_verified && across;
    let rechecks_reported = matches!(known, SwapBridgeOutcome::DeliveredReported { .. }) && across;
    explicit && (!known.is_final() || rechecks_refund || rechecks_reported)
}

/// What tracking an Across deposit's fill needs, apart from the record that holds the
/// deposit: a Bridge order's on its origin chain's owner, or a Public-paid swap's on its
/// destination chain's. The origin chain and the deposit id are in `fill`.
struct BridgeTracking<'a> {
    /// The record the outcome is written to, as it was read.
    record: &'a ExecutorRecord,
    target: OutcomeTarget,
    fill: ExpectedFill,
    /// The recorded outcome.
    known: Option<SwapBridgeOutcome>,
    /// The chain the fill is read on.
    destination: &'a EffectiveChainConfig,
    /// That chain's endpoints, when it is this owner's own. Otherwise they are opened for the
    /// read.
    endpoints: Option<&'a ObservationEndpoints>,
}

/// Where a tracked deposit's outcome is written in its record.
#[derive(Clone, Copy)]
enum OutcomeTarget {
    /// The Bridge order of a stealth account, on its origin chain.
    Order(OrderUid),
    /// The use of a swap paid from a Public account, on its destination chain.
    PublicSwap(SwapUseId),
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

/// [`read_fill`] through the destination chain's endpoints, each tried once, until one reads
/// the block. An identity check can admit an endpoint after the first ones were tried, so
/// while some weren't tried the later admissions are waited for, [`LATE_ADMISSION_WAIT`] in
/// all. When none reads the block, `Err` counts the endpoints that answered that whole-block
/// receipts are unsupported.
async fn read_fill_from_endpoints(
    endpoints: &ObservationEndpoints,
    destination: &EffectiveChainConfig,
    number: u64,
    spoke_pool: Option<Address>,
    fill: &ExpectedFill,
    shield: Option<ExpectedShield>,
) -> Result<FillRead, usize> {
    let configured = destination.rpc_route.endpoints().len();
    // By pool index, so that each configured endpoint is tried and counted once.
    let mut tried = BTreeSet::new();
    let mut unsupported = 0;
    let mut deadline = None;
    let mut providers = endpoints.providers().await;
    loop {
        for endpoint in providers {
            if !tried.insert(endpoint.index) {
                continue;
            }
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_bridge_fill",
                Box::pin(read_fill(
                    &endpoint.provider,
                    destination.finality_depth,
                    number,
                    spoke_pool,
                    fill,
                    shield,
                )),
            )
            .instrument(span)
            .await;
            match result {
                Ok(read) => {
                    endpoints.succeeded(&endpoint);
                    return Ok(read);
                }
                Err(error) => {
                    if matches!(
                        error.downcast_ref::<BlockReceiptsError>(),
                        Some(BlockReceiptsError::Unsupported)
                    ) {
                        unsupported += 1;
                    }
                    endpoints.failed(&endpoint, &error);
                }
            }
        }
        if tried.len() >= configured {
            return Err(unsupported);
        }
        let until =
            *deadline.get_or_insert_with(|| tokio::time::Instant::now() + LATE_ADMISSION_WAIT);
        providers = endpoints.later_providers(&tried, until).await;
        if providers.is_empty() {
            return Err(unsupported);
        }
    }
}

/// The fill that delivers an Across deposit.
struct ExpectedFill {
    origin_chain: u64,
    deposit_id: U256,
    /// The stealth account that deposited, or the Public account of a swap it pays.
    depositor: Address,
    delivery: BridgeDelivery,
    /// The deposit's terms, with the amounts its event recorded.
    terms: AcrossOrderTerms,
}

impl ExpectedFill {
    /// The event keeps the signed output amount. The executed one can be higher: a slow fill
    /// pays the deposit less the LP fee. It is the emitter's claim, trusted only from the
    /// pinned pool. The recipient is the delivery's receiver, or Across's
    /// handler for a private delivery, whose message hash is then the signed message's. An
    /// empty message has a zero hash.
    fn matches(&self, fill: &SpokePool::FilledRelay) -> bool {
        let recipient = address_to_bytes32(self.terms.deposit_recipient(self.delivery));
        let message_hash = self.terms.deposit_message_hash();
        fill.originChainId == U256::from(self.origin_chain)
            && fill.depositId == self.deposit_id
            && fill.depositor == address_to_bytes32(self.depositor)
            && fill.recipient == recipient
            && fill.inputToken == address_to_bytes32(self.terms.input_token)
            && fill.inputAmount == self.terms.input_amount
            && fill.outputToken == address_to_bytes32(self.terms.output_token)
            && fill.outputAmount == self.terms.output_amount
            && fill.messageHash == message_hash
            // A relayer may fill with a depositor-signed update; only the signed receiver and
            // message, and at least the signed amount, deliver.
            && fill.relayExecutionInfo.updatedRecipient == recipient
            && fill.relayExecutionInfo.updatedMessageHash == message_hash
            && fill.relayExecutionInfo.updatedOutputAmount >= self.terms.output_amount
    }

    /// The value of a public delivery's payment in `logs`, one receipt's: a transfer of the
    /// output token, emitted by that token, of at least the signed amount to the receiver. The
    /// sender isn't constrained: a fast fill pays from the relayer and a slow fill from the pool.
    /// A private delivery's fill pays the handler, so it has none.
    ///
    /// A relayer may batch payments, so several transfers can qualify. The smallest is taken:
    /// it never overstates the delivery and may understate it, and it doesn't establish the
    /// deposit's exact executed total.
    fn paid_to_receiver(&self, logs: &[Log]) -> Option<U256> {
        if self.delivery.is_private() {
            return None;
        }
        logs.iter()
            .filter(|log| log.address() == self.terms.output_token)
            .filter_map(|log| Some(log.log_decode::<Transfer>().ok()?.inner.data))
            .filter(|transfer| {
                transfer.to == self.delivery.receiver && transfer.value >= self.terms.output_amount
            })
            .map(|transfer| transfer.value)
            .min()
    }

    /// What the matching fill's receipt establishes. `executed` is the fill's executed amount
    /// and `logs` are its receipt's. Without `shield` the fill itself delivers. With it, the
    /// handler must have passed at least the signed amount of the destination token to the
    /// destination stealth account, or the fill is not delivery evidence. A later transfer of
    /// that token from the account to Railgun shows its shield ran; Railgun's fee goes to its
    /// treasury in a separate transfer. Without that transfer the account holds the token.
    fn outcome(
        &self,
        shield: Option<ExpectedShield>,
        block: BlockNumHash,
        transaction_hash: B256,
        executed: U256,
        logs: &[Log],
    ) -> Option<SwapBridgeOutcome> {
        let Some(shield) = shield else {
            return Some(SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: executed,
                shielded: false,
            });
        };
        let account = self.delivery.receiver;
        let mut transfers = logs
            .iter()
            .filter(|log| log.address() == self.terms.output_token)
            .filter_map(|log| Some(log.log_decode::<Transfer>().ok()?.inner.data));
        let received = transfers
            .find(|transfer| {
                transfer.from == shield.handler
                    && transfer.to == account
                    && transfer.value >= self.terms.output_amount
            })?
            .value;
        let shielded = transfers.any(|transfer| {
            transfer.from == account && transfer.to == shield.railgun && !transfer.value.is_zero()
        });
        Some(if shielded {
            // The gross amount the handler passed to the account, before Railgun's shield fee.
            // It is what the store compares with the approved destination minimum.
            SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: received,
                shielded: true,
            }
        } else {
            SwapBridgeOutcome::HeldOnDestination {
                block,
                transaction_hash,
                amount: received,
            }
        })
    }
}

/// Where a private delivery's fill is passed on and shielded on the destination chain.
#[derive(Clone, Copy)]
struct ExpectedShield {
    /// Across's handler, the fill's recipient.
    handler: Address,
    /// The destination chain's Railgun contract.
    railgun: Address,
}

/// The transfer that refunds an Across deposit to its depositor on the chain it was made on.
struct ExpectedRefund {
    spoke_pool: Address,
    token: Address,
    /// The depositor: the stealth account, or the Public account of a swap it pays.
    payee: Address,
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
                    && transfer.to == self.payee
                    && transfer.value >= self.amount
            })
    }
}

/// What reading a fill block established.
enum FillRead {
    /// A candidate fill's outcome.
    Found(SwapBridgeOutcome),
    /// The block is final, was read whole and is still canonical, and holds no candidate.
    NoCandidate,
    /// Not final yet, left the canonical chain during the read, or a private fill that didn't
    /// reach the destination stealth account.
    Unresolved,
}

/// Match `fill` in the finalized receipts of block `number`, sending only block identifiers.
/// Evidence belongs to a receipt, so a receipt is one candidate however many matching events
/// it holds. One matching event from `spoke_pool`, the destination chain's pinned pool, makes
/// its receipt a candidate for that event's executed amount, whatever other emitters log
/// beside it. Without one, a receipt with a matching event from any emitter is a candidate
/// when it pays the receiver, for the amount paid. See [`ExpectedFill::paid_to_receiver`].
/// Anyone can emit a matching event, so a receipt with neither is ignored and doesn't make the
/// block ambiguous. Two candidate receipts do, as do two matching events of the pinned pool in
/// one receipt. `shield` is given for a private delivery. See [`ExpectedFill::outcome`].
async fn read_fill(
    provider: &DynProvider,
    finality_depth: u64,
    number: u64,
    spoke_pool: Option<Address>,
    fill: &ExpectedFill,
    shield: Option<ExpectedShield>,
) -> Result<FillRead> {
    let Some((identity, receipts)) = finalized_receipts(provider, finality_depth, number).await?
    else {
        return Ok(FillRead::Unresolved);
    };
    let mut found = None;
    for receipt in receipts {
        let logs = receipt.inner.logs();
        let mut matched = false;
        let mut pinned = None;
        for log in logs {
            let Ok(event) = log.log_decode::<SpokePool::FilledRelay>() else {
                continue;
            };
            let event = event.inner.data;
            if !fill.matches(&event) {
                continue;
            }
            matched = true;
            if Some(log.address()) == spoke_pool {
                if pinned.is_some() {
                    return Err(eyre!("fill block contains ambiguous fill evidence"));
                }
                pinned = Some(event.relayExecutionInfo.updatedOutputAmount);
            }
        }
        // An unpinned event's amount is its emitter's claim, so the payment's value stands in.
        let Some(executed) =
            pinned.or_else(|| matched.then(|| fill.paid_to_receiver(logs)).flatten())
        else {
            continue;
        };
        if found.is_some() {
            return Err(eyre!("fill block contains ambiguous fill evidence"));
        }
        found = Some(fill.outcome(shield, identity, receipt.transaction_hash(), executed, logs));
    }
    // A block without a candidate is reported on, so it is rechecked like one with a fill.
    if !still_canonical(provider, identity).await? {
        return Ok(FillRead::Unresolved);
    }
    Ok(match found {
        Some(Some(outcome)) => FillRead::Found(outcome),
        Some(None) => FillRead::Unresolved,
        None => FillRead::NoCandidate,
    })
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
pub(super) async fn finalized_receipts(
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
    .map_err(|error| {
        // Kept apart from a failed read: a chain no endpoint serves whole-block receipts for
        // has its delivery reported.
        if matches!(error, BlockReceiptsError::Unsupported) {
            eyre::Report::new(error)
        } else {
            eyre!("whole-block bridge receipts are incomplete or unavailable")
        }
    })?;
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
pub(super) async fn still_canonical(
    provider: &DynProvider,
    identity: BlockNumHash,
) -> Result<bool> {
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
        Some(SwapBridgeOutcome::HeldOnDestination { .. }) => "held_on_destination",
    }
}
