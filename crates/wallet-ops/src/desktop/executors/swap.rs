use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider as _;
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;
use tracing::Instrument as _;

use super::execution::OperationReservation;
use super::recovery::{PaidExecutionPurpose, recovery_gas_limits};
use super::{
    ExecutorDelivery, ExecutorOwner, ExecutorPaidRecoveryOutcome, ExecutorPrivateFeeLimitExceeded,
    ExecutorReconciliationReport, ExecutorRecoveryExecution, ExecutorRecoveryFeeEstimate,
    PreparedExecutorOperation,
};
use crate::desktop::executor_discovery::{execution_nonce_with_code, matches_executor_delegation};
use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::settings::{ExecutorProfile, SwapTokenEligibility};
use crate::vault::{
    BridgeDelivery, ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadPurpose,
    ExecutorPayloadStatus, ExecutorRecord, SwapApproval, SwapDelivery, SwapDestinationRecord,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, PublicBroadcasterCandidate,
    TransactionGenerationProgressSender, WakuClient, WalletSession,
};

mod bridge;
mod destination;
mod gas;
mod observation;
mod order;
mod recovery;
mod settlement;
mod simulation;
pub use bridge::{
    BridgeLegPrice, SwapBridgeClients, SwapBridgeQuote, SwapBridgeRoute, SwapPrivateBridgeQuote,
};
pub use observation::{SwapOrderState, swap_order_state};
pub use order::{
    SwapAccountCandidate, SwapAmountPlan, SwapAmountRequest, SwapDestinationContext, SwapInputPlan,
    SwapOrderOutcome, SwapOrderRequest, SwapPrice, SwapReview, SwapReviewChange, SwapReviewRequest,
    swap_submission_outcome,
};
#[cfg(test)]
pub(crate) use order::{
    SwapDestinationSigning, SwapOrderSigning, SwapOutputPoiSink, plan_swap_inputs,
    price_swap_review, reusable_swap_proof, swap_invalidation,
};
pub(super) use recovery::swap_recovery_call_bound;
#[cfg(test)]
pub(crate) use recovery::{swap_cancellation_admitted, swap_recovery_calls};
pub(crate) use settlement::Transfer;

/// Neutral purpose recorded for swap executors; the tokens stay in the setup approval and
/// the order terms.
const SWAP_PURPOSE_SUMMARY: &str = "Private swap";

/// Whether `record` belongs to a private swap, during setup or with orders.
#[must_use]
pub fn is_swap_record(record: &ExecutorRecord) -> bool {
    record.swap().is_some() || record.purpose_summary() == Some(SWAP_PURPOSE_SUMMARY)
}

/// Whether `record` is the destination stealth account of a private Bridge swap.
#[must_use]
pub const fn is_swap_destination_record(record: &ExecutorRecord) -> bool {
    record.swap_destination().is_some()
}

/// Delivery of an approved setup. The fee ceiling is the reviewed maximum.
pub struct SwapSetupRequest {
    pub maximum_private_fee: U256,
    pub session: Arc<WalletSession>,
    pub authorization: DesktopPrivateSpendAuthorization,
    pub waku: Arc<WakuClient>,
    pub verify_proof: bool,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
}

/// What [`prepare_private_bridge_setup`] reserves: the swap's own stealth account and its
/// destination stealth account, each with a broadcaster and an authorization for its chain.
pub struct PrivateBridgeSetupPreparation<'a> {
    pub operation: ExecutorOperationId,
    pub destination_operation: ExecutorOperationId,
    pub candidate: PublicBroadcasterCandidate,
    pub destination_candidate: PublicBroadcasterCandidate,
    /// The reviewed terms. Its Bridge delivery's `receiver` is a placeholder until the
    /// destination account's address is derived.
    pub approval: SwapApproval,
    pub authorization: &'a DesktopPrivateSpendAuthorization,
    pub destination_authorization: &'a DesktopPrivateSpendAuthorization,
}

/// Both reserved stealth accounts of a private Bridge swap.
pub struct PreparedPrivateBridgeSetup {
    pub origin: PreparedExecutorOperation,
    pub destination: PreparedExecutorOperation,
    /// The approval saved with the swap's record. Its Bridge delivery's `receiver` is the
    /// destination stealth account.
    pub approval: SwapApproval,
}

/// Reserve both stealth accounts of a private Bridge swap in the order the records need:
/// the destination account first, then the swap's own account with the approval that names the
/// destination account as receiver and the link to it. A crash in between leaves a reserved
/// destination account that no swap references, which the destination chain's owner retires
/// on its next load.
/// A retry with the same operations resumes the destination account. Once the swap's own record
/// exists, retry its setup with [`ExecutorOwner::resume_swap_setup`] instead.
pub async fn prepare_private_bridge_setup(
    origin: &ExecutorOwner,
    destination: &ExecutorOwner,
    request: PrivateBridgeSetupPreparation<'_>,
) -> Result<PreparedPrivateBridgeSetup> {
    let PrivateBridgeSetupPreparation {
        operation,
        destination_operation,
        candidate,
        destination_candidate,
        mut approval,
        authorization,
        destination_authorization,
    } = request;
    let delivery = match approval.delivery {
        SwapDelivery::Bridge(delivery)
            if delivery.is_private()
                && delivery.has_valid_private_delivery()
                && delivery.destination_chain == destination.chain.chain_id
                && delivery.destination_chain != origin.chain.chain_id =>
        {
            delivery
        }
        _ => {
            return Err(eyre!(
                "the swap's approval is not a private Bridge delivery to this destination network"
            ));
        }
    };
    if approval.bounds.destination_setup_fee.is_none() {
        return Err(eyre!(
            "the swap's approval has no destination setup fee limit"
        ));
    }
    if !origin.view.is_same_wallet_session(&destination.view) {
        return Err(eyre!(
            "the destination network belongs to another wallet session"
        ));
    }
    let prepared_destination = Box::pin(destination.prepare_swap_destination_setup(
        destination_operation,
        destination_candidate,
        SwapDestinationRecord {
            origin_chain: origin.chain.chain_id,
            origin_operation: operation,
            destination_token: delivery.destination_token,
            outcome: None,
        },
        destination_authorization,
    ))
    .await?;
    approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
        receiver: prepared_destination.context().executor,
        ..delivery
    });
    let prepared_origin = Box::pin(origin.prepare_swap_setup(
        operation,
        candidate,
        approval.clone(),
        Some(destination_operation),
        authorization,
    ))
    .await?;
    Ok(PreparedPrivateBridgeSetup {
        origin: prepared_origin,
        destination: prepared_destination,
        approval,
    })
}

/// Submit both setups of a private Bridge swap at the same time, each through its own owner and
/// session. Each result stands by itself: a failed setup is retried alone with
/// `resume_swap_setup` or `prepare_swap_destination_setup`, without repeating the other.
pub async fn submit_private_bridge_setups(
    origin: (&ExecutorOwner, &PreparedExecutorOperation, SwapSetupRequest),
    destination: (&ExecutorOwner, &PreparedExecutorOperation, SwapSetupRequest),
) -> (
    Result<ExecutorPaidRecoveryOutcome>,
    Result<ExecutorPaidRecoveryOutcome>,
) {
    tokio::join!(
        Box::pin(origin.0.submit_swap_setup(origin.1, origin.2)),
        Box::pin(
            destination
                .0
                .submit_swap_setup(destination.1, destination.2)
        ),
    )
}

/// A swap executor whose setup canonically installed the accepted delegation and
/// consumed its execution nonce. Order preparation for the swap starts here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegatedSwapExecutor {
    operation: ExecutorOperationId,
    executor: Address,
    delegate: Address,
    setup_payload: B256,
    observed: ExecutorNonceObservation,
}

impl DelegatedSwapExecutor {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn executor(&self) -> Address {
        self.executor
    }
    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.delegate
    }
    /// The winning setup, recorded in the swap's terms with its first order.
    #[must_use]
    pub const fn setup_payload(&self) -> B256 {
        self.setup_payload
    }
    /// The confirmed execution nonce observation the delegation was checked at.
    #[must_use]
    pub const fn observed(&self) -> ExecutorNonceObservation {
        self.observed
    }
}

/// The executor a swap's order is planned and quoted for. Planning and review need only its
/// address, its delegate, and a nonce hint, so they also run before the setup is confirmed.
/// Execution preparation refreshes that hint. Signing requires the confirmed delegation this
/// handle carries only when made from [`DelegatedSwapExecutor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapExecutor {
    /// `None` for a preview before any executor is reserved.
    operation: Option<ExecutorOperationId>,
    executor: Address,
    delegate: Address,
    expected_pre_hook_nonce: U256,
    setup: SwapExecutorSetup,
    reused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwapExecutorSetup {
    Pending,
    /// Recorded setup is sufficient for a quote, but cannot authorize signing.
    Recorded,
    Checked(DelegatedSwapExecutor),
}

impl SwapExecutor {
    /// A swap executor reserved for its setup. The setup takes the fresh executor's current
    /// nonce `k0`, so the pre-hook is expected at `k0 + 1`.
    pub fn reserved(prepared: &PreparedExecutorOperation) -> Result<Self> {
        if prepared.is_recovery() {
            return Err(eyre!(
                "executor preparation does not belong to a swap setup"
            ));
        }
        let context = prepared.context();
        Ok(Self {
            operation: Some(prepared.operation()),
            executor: context.executor,
            delegate: context.delegate,
            expected_pre_hook_nonce: context
                .execution_nonce
                .checked_add(U256::ONE)
                .ok_or_else(|| eyre!("executor nonce is exhausted"))?,
            setup: SwapExecutorSetup::Pending,
            reused: false,
        })
    }

    #[must_use]
    pub const fn operation(&self) -> Option<ExecutorOperationId> {
        self.operation
    }
    #[must_use]
    pub const fn executor(&self) -> Address {
        self.executor
    }
    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.delegate
    }
    #[must_use]
    pub const fn expected_pre_hook_nonce(&self) -> U256 {
        self.expected_pre_hook_nonce
    }
    /// The confirmed delegation, required to sign.
    #[must_use]
    pub const fn delegated(&self) -> Option<DelegatedSwapExecutor> {
        match self.setup {
            SwapExecutorSetup::Checked(delegated) => Some(delegated),
            SwapExecutorSetup::Pending | SwapExecutorSetup::Recorded => None,
        }
    }
    /// Whether the account still needs setup. Recorded setup is only a preview;
    /// execution preparation checks its delegation before constructing a signing plan.
    #[must_use]
    pub const fn requires_setup(&self) -> bool {
        matches!(self.setup, SwapExecutorSetup::Pending)
    }

    /// The user selected this account explicitly instead of allocating a fresh one.
    #[must_use]
    pub const fn is_reused(&self) -> bool {
        self.reused
    }
}

impl From<DelegatedSwapExecutor> for SwapExecutor {
    fn from(delegated: DelegatedSwapExecutor) -> Self {
        Self {
            operation: Some(delegated.operation),
            executor: delegated.executor,
            delegate: delegated.delegate,
            expected_pre_hook_nonce: delegated.observed.nonce(),
            setup: SwapExecutorSetup::Checked(delegated),
            reused: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSetupStatus {
    /// No setup outcome is canonical at the confirmed block yet.
    Pending,
    /// Every issued setup reverted or lost its nonce. Resume with the same executor.
    Failed,
    /// A setup transaction succeeded but its authorization was not applied.
    MissingDelegation,
    Delegated(DelegatedSwapExecutor),
}

/// Evaluate a reconciled swap record against the account code read at its nonce
/// observation block. Delegation needs the accepted designator, an executed setup,
/// and an execution nonce past that setup's nonce; no single signal suffices.
#[must_use]
pub fn swap_setup_status(
    record: &ExecutorRecord,
    code_block: BlockNumHash,
    code: &[u8],
    profile: ExecutorProfile,
) -> SwapSetupStatus {
    let (Some(observed), Some(executor)) = (record.nonce_observation(), record.address()) else {
        return SwapSetupStatus::Pending;
    };
    if observed.block() != code_block || record.delegate() != profile.delegate() {
        return SwapSetupStatus::Pending;
    }
    let delegated = matches_executor_delegation(code, profile);
    let (mut failed, mut pending) = (false, false);
    for setup in record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
    {
        match record.payload_status(setup.hash()) {
            Some(ExecutorPayloadStatus::Executed)
                if delegated && observed.nonce() > setup.nonce() =>
            {
                return SwapSetupStatus::Delegated(DelegatedSwapExecutor {
                    operation: record.operation(),
                    executor,
                    delegate: record.delegate(),
                    setup_payload: setup.hash(),
                    observed,
                });
            }
            // A call to an undelegated account succeeds without the setup's effects.
            Some(ExecutorPayloadStatus::Executed | ExecutorPayloadStatus::MissingEffects)
                if !delegated =>
            {
                return SwapSetupStatus::MissingDelegation;
            }
            Some(ExecutorPayloadStatus::Reverted | ExecutorPayloadStatus::Invalidated { .. }) => {
                failed = true;
            }
            _ => pending = true,
        }
    }
    if failed && !pending {
        SwapSetupStatus::Failed
    } else {
        SwapSetupStatus::Pending
    }
}

/// Whether a swap without orders records an executed setup for `profile`'s delegate: the
/// recorded winner of a setup nonce, which may be an earlier attempt that invalidated its
/// replacement. This is recorded progress under the wallet's issuance assumptions. The
/// inclusion's receipt check verifies the Railgun-emitted effects, not the authorization,
/// the delegation designator, or the nonce, so this never authorizes signing; order
/// preparation checks the account afresh with [`swap_setup_status`].
#[must_use]
pub fn swap_setup_recorded_executed(record: &ExecutorRecord, profile: ExecutorProfile) -> bool {
    record.swap().is_none()
        && record.delegate() == profile.delegate()
        && record.issued().iter().any(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::Operation
                && record.recorded_payload_status(payload.hash())
                    == Some(ExecutorPayloadStatus::Executed)
        })
}

impl ExecutorOwner {
    /// Whether this session's available private notes can pay a fresh setup with `candidate`.
    /// Uses the same estimate as review; an unavailable RPC or invalid offer remains an error.
    pub async fn can_fund_swap_setup(
        &self,
        session: &WalletSession,
        candidate: PublicBroadcasterCandidate,
    ) -> Result<bool> {
        match self.estimate_swap_setup_fee(session, None, candidate).await {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.downcast_ref::<crate::BuildError>(),
                    Some(
                        crate::BuildError::InsufficientBalance(_)
                            | crate::BuildError::InsufficientFeeTokenBalance(_)
                    )
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Preview a new setup's private fee, or a retry using that operation's own notes.
    pub async fn estimate_swap_setup_fee(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
        candidate: PublicBroadcasterCandidate,
    ) -> Result<ExecutorRecoveryFeeEstimate> {
        self.ensure_active()?;
        self.require_fee_session(session, PaidExecutionPurpose::SwapSetup)?;
        let record = match operation {
            Some(operation) => self.swap_account_record(operation)?,
            None => None,
        };
        let profile = self.swap_setup_profile(record.as_ref())?;
        let inputs = self.spendable_swap_inputs(session, operation)?;
        self.estimate_paid_execution_fee(
            PaidExecutionPurpose::SwapSetup,
            profile,
            candidate,
            &inputs,
            |buffer| setup_gas_budget(self.chain.chain_id, buffer),
        )
        .await
    }

    /// A stand-in executor for quoting a swap before its executor is reserved. Reserving
    /// derives the address under spend authorization, which the review asks for only after the
    /// quote. The random address has no code and no balance, like a fresh executor, and links
    /// nothing if the swap is abandoned. Nothing can be signed for it.
    pub fn swap_preview_executor(&self) -> Result<SwapExecutor> {
        self.ensure_active()?;
        let profile = self.swap_executor_profile()?;
        let mut address = [0; 20];
        getrandom::fill(&mut address).map_err(|_| eyre!("randomness is unavailable"))?;
        Ok(SwapExecutor {
            operation: None,
            executor: Address::from(address),
            delegate: profile.delegate(),
            // A fresh executor's setup takes nonce 0.
            expected_pre_hook_nonce: U256::ONE,
            setup: SwapExecutorSetup::Pending,
            reused: false,
        })
    }

    /// Quote a setup retry using its existing account and its own reserved fee notes.
    /// This is a preview only; signing still requires the confirmed delegation.
    pub fn swap_setup_preview(&self, operation: ExecutorOperationId) -> Result<SwapExecutor> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        if record.is_retired() || record.is_swap_setup_stopped() || record.swap().is_some() {
            return Err(eyre!("this swap cannot retry setup"));
        }
        let Some(executor) = record.address() else {
            return self.swap_preview_executor();
        };
        let setup_nonce = record
            .issued()
            .iter()
            .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
            .map(crate::vault::IssuedExecutorPayload::nonce)
            .max()
            .unwrap_or(U256::ZERO);
        Ok(SwapExecutor {
            operation: Some(operation),
            executor,
            delegate: record.delegate(),
            expected_pre_hook_nonce: setup_nonce
                .checked_add(U256::ONE)
                .ok_or_else(|| eyre!("executor nonce is exhausted"))?,
            setup: SwapExecutorSetup::Pending,
            reused: false,
        })
    }

    /// Persist terms the user authorized for a reserved swap's setup, replacing the approval
    /// saved with it. Call this only after the user approved a new review. The order is placed
    /// with them once the setup is confirmed, also after a restart.
    pub fn record_swap_approval(
        &self,
        operation: ExecutorOperationId,
        approval: SwapApproval,
    ) -> Result<()> {
        self.ensure_active()?;
        self.swap_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        self.store.record_swap_approval(operation, approval)?;
        self.notify_change();
        Ok(())
    }

    /// Remove a setup from active swaps, preserving the account for observation and recovery.
    /// Stopping a swap also stops its destination stealth account on that chain's next load;
    /// call this on the destination chain's owner to stop that account at once.
    pub fn stop_swap_setup(&self, operation: ExecutorOperationId) -> Result<()> {
        self.ensure_active()?;
        self.swap_setup_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        self.store.stop_swap_setup(operation)?;
        self.notify_change();
        Ok(())
    }

    /// Reserve a fresh executor for a new swap, with the terms the user approved for it. The
    /// record is created holding `approval`, whose pair and delivery bind the first order. The
    /// operation must be unused, so no swap ever takes over another operation's executor.
    /// Setup delivery is broadcaster-only. A private Bridge delivery needs
    /// `destination_operation`, its destination stealth account already reserved on the
    /// destination chain, and no other delivery takes one.
    pub async fn prepare_swap_setup(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        approval: SwapApproval,
        destination_operation: Option<ExecutorOperationId>,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
        let private_bridge = matches!(
            approval.delivery,
            SwapDelivery::Bridge(delivery) if delivery.is_private()
        );
        if private_bridge != destination_operation.is_some() {
            return Err(eyre!(
                "a destination stealth account belongs to a private Bridge delivery only"
            ));
        }
        let swap = self
            .chain
            .swap_profile()
            .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))?;
        let tokens = approval
            .tokens
            .ok_or_else(|| eyre!("the swap's approval has no token pair"))?;
        if swap.pair_eligibility(tokens.sell, tokens.buy, approval.delivery)
            != SwapTokenEligibility::Eligible
        {
            return Err(eyre!("this token pair is not eligible for private swaps"));
        }
        if self.swap_record(operation)?.is_some() {
            return Err(eyre!(
                "this swap already has an executor; resume it instead"
            ));
        }
        // Recovery inspects the sell token. `record_swap_attempt` adds the buy token for a
        // Reshield order, the only kind that pays it to the executor.
        self.prepare_reserved_operation(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            authorization,
            &[ExecutorAsset::Erc20(tokens.sell)],
            OperationReservation::Operation {
                purpose_summary: Some(SWAP_PURPOSE_SUMMARY),
                swap_approval: Some(&approval),
                destination_operation,
            },
        )
        .await
    }

    /// Reserve the destination stealth account of a private Bridge swap on this chain, the
    /// destination chain, and derive its address. Call it before the swap's own setup is prepared,
    /// so the approval saved with that setup names this account. An account already reserved
    /// for `operation` is resumed if it serves the same swap and token.
    pub async fn prepare_swap_destination_setup(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        destination: SwapDestinationRecord,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
        self.swap_destination_profile()?;
        if let Some(record) = self.swap_account_record(operation)? {
            if !is_swap_destination_record(&record) {
                return Err(eyre!("this executor does not belong to a swap destination"));
            }
            if record.is_swap_setup_stopped() {
                return Err(eyre!("this swap was stopped"));
            }
        }
        // Recovery inspects the destination token, which a fill without its shield leaves here.
        self.prepare_reserved_operation(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            authorization,
            &[ExecutorAsset::Erc20(destination.destination_token)],
            OperationReservation::SwapDestination(destination),
        )
        .await
    }

    /// Retry the setup of an existing swap, or of a swap's destination stealth account on this
    /// chain, with the executor it already reserved.
    pub async fn resume_swap_setup(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
        let record = self
            .swap_setup_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        self.swap_setup_profile(Some(&record))?;
        if record.is_swap_setup_stopped() {
            return Err(eyre!("this swap was stopped"));
        }
        // The reservation is returned only for the links its record was created with.
        let reservation = match record.swap_destination() {
            Some(destination) => OperationReservation::SwapDestination(destination),
            None => OperationReservation::Operation {
                purpose_summary: Some(SWAP_PURPOSE_SUMMARY),
                swap_approval: None,
                destination_operation: record.destination_operation(),
            },
        };
        self.prepare_reserved_operation(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            authorization,
            record.assets(),
            reservation,
        )
        .await
    }

    /// Pay the selected broadcaster privately for an `execute` with no actions,
    /// carrying only this executor's delegation authorization. The payload is
    /// durable before handoff; completion comes from `observe_swap_setup`. A destination
    /// stealth account's fee ceiling may not exceed the one approved with its swap.
    pub async fn submit_swap_setup(
        &self,
        prepared: &PreparedExecutorOperation,
        request: SwapSetupRequest,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        self.while_active(Box::pin(self.submit_swap_setup_active(prepared, request)))
            .await
    }

    async fn submit_swap_setup_active(
        &self,
        prepared: &PreparedExecutorOperation,
        request: SwapSetupRequest,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        self.require_fee_session(&request.session, PaidExecutionPurpose::SwapSetup)?;
        let record = self
            .swap_setup_record(prepared.operation())?
            .ok_or_else(|| eyre!("executor preparation does not belong to a swap"))?;
        self.swap_setup_profile(Some(&record))?;
        let ExecutorDelivery::PublicBroadcaster(candidate) = prepared.delivery() else {
            return Err(eyre!("swap setup requires a compatible broadcaster"));
        };
        if is_swap_destination_record(&record) {
            self.require_swap_destination_setup_fee(
                record.operation(),
                candidate.token,
                request.maximum_private_fee,
            )?;
        }
        // The broadcaster route admits only one authorization, signed by the
        // transaction's executor for the accepted delegate, before publishing.
        self.submit_paid_execution(
            PaidExecutionPurpose::SwapSetup,
            prepared,
            candidate,
            &[],
            setup_gas_budget(self.chain.chain_id, self.chain.gas.gas_limit_buffer),
            request.maximum_private_fee,
            &request.session,
            request.authorization,
            &request.waku,
            request.verify_proof,
            request.progress_tx.as_ref(),
            request.response_timeout,
            request.republish_interval,
        )
        .await
    }

    /// Reconcile the setup over `range`, rechecking earlier inclusions, at the
    /// confirmed block, and read the executor's code at that same block. A retry
    /// of a delegated swap resumes from the returned executor.
    pub async fn observe_swap_setup(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<SwapSetupStatus> {
        if self.swap_account_record(operation)?.is_none() {
            return Err(eyre!("swap executor is unavailable"));
        }
        let report = trace_step("setup_history", self.reconcile_history(operation, range)).await?;
        self.check_swap_setup(&report).await
    }

    /// Confirm this chain's destination stealth account of a private Bridge swap at
    /// `confirmed`, before the swap's order is signed on `origin_chain`. The account must be
    /// delegated there, serve the swap `origin_operation`, and be `receiver`, the account the
    /// order's delivery names. Nothing is signed.
    pub(crate) async fn delegated_swap_destination(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
        origin_chain: u64,
        origin_operation: ExecutorOperationId,
        receiver: Address,
    ) -> Result<DelegatedSwapExecutor> {
        let serves = |record: &ExecutorRecord| {
            record.swap_destination().is_some_and(|destination| {
                destination.origin_chain == origin_chain
                    && destination.origin_operation == origin_operation
            }) && record.address() == Some(receiver)
                && record.swap().is_none()
                && !record.is_retired()
                && !record.is_swap_setup_stopped()
        };
        let unavailable = || {
            eyre!(
                "the stealth account on the destination network ({}) is unavailable for this swap",
                self.chain.name
            )
        };
        self.while_active(Box::pin(async {
            let record = self.swap_account_record(operation)?.ok_or_else(unavailable)?;
            if !serves(&record) {
                return Err(unavailable());
            }
            let range = confirmed..confirmed.saturating_add(1);
            let report =
                trace_step("destination_history", self.reconcile_history(operation, range)).await?;
            let SwapSetupStatus::Delegated(delegated) =
                trace_step("destination_setup", self.check_swap_setup(&report)).await?
            else {
                return Err(eyre!(
                    "the stealth account's setup on the destination network ({}) is not confirmed; finish or retry it first",
                    self.chain.name
                ));
            };
            let record = report.record();
            let _guard = self.lock_activity().await;
            self.require_record_unchanged(record)?;
            if !serves(record) {
                return Err(unavailable());
            }
            Ok(delegated)
        }))
        .await
    }

    async fn check_swap_setup(
        &self,
        report: &ExecutorReconciliationReport,
    ) -> Result<SwapSetupStatus> {
        let record = report.record();
        let (Some(observed), Some(executor)) = (record.nonce_observation(), record.address())
        else {
            return Ok(SwapSetupStatus::Pending);
        };
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("this swap executor's delegate is not supported"))?;
        // Only an executed or effect-less setup call depends on the account's code.
        let executed = record
            .issued()
            .iter()
            .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
            .any(|payload| {
                matches!(
                    record.payload_status(payload.hash()),
                    Some(ExecutorPayloadStatus::Executed | ExecutorPayloadStatus::MissingEffects)
                )
            });
        // The history's nonce read loaded the code; reuse it when read at the same block.
        let code = if !executed {
            Bytes::new()
        } else if let Some((_, code)) = report
            .code
            .as_ref()
            .filter(|(block, _)| *block == observed.block())
        {
            code.clone()
        } else {
            trace_step(
                "setup_delegation",
                self.while_active(code_at(&self.endpoints, executor, observed.block())),
            )
            .await?
        };
        Ok(swap_setup_status(record, observed.block(), &code, profile))
    }

    fn swap_executor_profile(&self) -> Result<ExecutorProfile> {
        self.chain
            .swap_profile()
            .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))?;
        self.chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))
    }

    /// A destination stealth account needs the bridge's parameters on its chain and an
    /// accepted executor, and places no orders there.
    fn swap_destination_profile(&self) -> Result<ExecutorProfile> {
        self.chain
            .bridge_profile()
            .ok_or_else(|| eyre!("private Bridge delivery is unavailable on this chain"))?;
        self.chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))
    }

    /// The profile a setup needs: a destination stealth account's, or otherwise a swap's.
    fn swap_setup_profile(&self, record: Option<&ExecutorRecord>) -> Result<ExecutorProfile> {
        if record.is_some_and(is_swap_destination_record) {
            self.swap_destination_profile()
        } else {
            self.swap_executor_profile()
        }
    }

    /// Refuse a destination setup's private fee ceiling above the destination setup fee approved
    /// with its swap, which is saved in the swap's record on its own chain.
    pub(crate) fn require_swap_destination_setup_fee(
        &self,
        operation: ExecutorOperationId,
        fee_token: Address,
        maximum_private_fee: U256,
    ) -> Result<()> {
        self.ensure_active()?;
        let approved = self
            .store
            .swap_destination_origin(operation)?
            .and_then(|origin| origin.swap_approval()?.bounds.destination_setup_fee)
            .ok_or_else(|| eyre!("this destination stealth account has no approved setup fee"))?;
        if maximum_private_fee > approved {
            return Err(ExecutorPrivateFeeLimitExceeded::new(
                PaidExecutionPurpose::SwapSetup,
                fee_token,
                approved,
                maximum_private_fee,
            )
            .into());
        }
        Ok(())
    }

    fn swap_record(&self, operation: ExecutorOperationId) -> Result<Option<ExecutorRecord>> {
        let Some(record) = self.swap_account_record(operation)? else {
            return Ok(None);
        };
        if !is_swap_record(&record) {
            return Err(eyre!("this executor does not belong to a swap"));
        }
        Ok(Some(record))
    }

    /// A record whose account a swap setup delegates: a swap's, or a swap's destination
    /// stealth account on this chain.
    fn swap_setup_record(&self, operation: ExecutorOperationId) -> Result<Option<ExecutorRecord>> {
        let Some(record) = self.swap_account_record(operation)? else {
            return Ok(None);
        };
        if !is_swap_record(&record) && !is_swap_destination_record(&record) {
            return Err(eyre!("this executor does not belong to a swap"));
        }
        Ok(Some(record))
    }

    fn swap_account_record(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<Option<ExecutorRecord>> {
        self.ensure_active()?;
        Ok(self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation))
    }

    /// User-selected reuse needs current account state, not another history scan when all
    /// swaps already have finalized delivery. Reads are outside activity; commit checks
    /// that no signing, recovery or observation changed the snapshot meanwhile.
    async fn refresh_settled_swap(&self, previous: &ExecutorRecord) -> Result<SwapExecutor> {
        let executor = previous
            .address()
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let mut chain = self.chain.clone();
        chain
            .railgun
            .as_mut()
            .ok_or_else(|| eyre!("chain does not support Railgun"))?
            .deployment
            .relay_adapt_7702_contract = previous.delegate();
        for endpoint in self.endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "reuse_current_account",
                self.while_active(async {
                    let head = endpoint.provider.get_block_number().await?;
                    let number = head
                        .checked_sub(chain.finality_depth)
                        .ok_or_else(|| eyre!("waiting for the account's safety cutoff"))?;
                    if !previous.settled_swaps_at(number) {
                        return Err(eyre!(
                            "the account's finalized history is ahead of this endpoint"
                        ));
                    }
                    let block = endpoint
                        .provider
                        .get_block_by_number(number.into())
                        .await?
                        .ok_or_else(|| eyre!("account block is unavailable"))?;
                    if block.header.number != number {
                        return Err(eyre!("account block does not match its height"));
                    }
                    let block = BlockNumHash::new(number, block.header.hash);
                    let code = endpoint
                        .provider
                        .get_code_at(executor)
                        .block_id(BlockId::hash_canonical(block.hash))
                        .await?;
                    let profile = ExecutorProfile::accepted(chain.chain_id, previous.delegate())
                        .ok_or_else(|| eyre!("the account's delegate is unsupported"))?;
                    if !matches_executor_delegation(&code, profile) {
                        return Err(eyre!("the account's delegation changed"));
                    }
                    let nonce = execution_nonce_with_code(
                        &endpoint.provider,
                        &chain,
                        executor,
                        BlockId::hash_canonical(block.hash),
                        Some(&code),
                        false,
                    )
                    .await
                    .ok_or_else(|| eyre!("the account's execution nonce is unavailable"))?;
                    let current = endpoint
                        .provider
                        .get_block_by_number(number.into())
                        .await?
                        .ok_or_else(|| eyre!("account block is unavailable"))?;
                    if current.header.hash != block.hash || current.header.number != number {
                        return Err(eyre!("account block changed during observation"));
                    }
                    Ok(ExecutorNonceObservation::new(block, nonce))
                }),
            )
            .instrument(span)
            .await;
            match result {
                Ok(observed) => {
                    self.endpoints.succeeded(&endpoint);
                    let _guard = self.lock_activity().await;
                    self.require_record_unchanged(previous)?;
                    let record = self.store.refresh_settled_swap_nonce(previous, observed)?;
                    self.notify_change();
                    // Use the retained setup and fresh nonce; no historical winner is added.
                    let setup = record
                        .issued()
                        .iter()
                        .find(|payload| {
                            payload.purpose() == ExecutorPayloadPurpose::Operation
                                && record.payload_status(payload.hash())
                                    == Some(ExecutorPayloadStatus::Executed)
                        })
                        .ok_or_else(|| eyre!("the account's setup is unavailable"))?;
                    return Ok(SwapExecutor::from(DelegatedSwapExecutor {
                        operation: record.operation(),
                        executor,
                        delegate: record.delegate(),
                        setup_payload: setup.hash(),
                        observed,
                    }));
                }
                Err(error) => self.endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "The stealth account could not be checked. Try again."
        ))
    }
}

#[cfg(test)]
impl ExecutorOwner {
    /// Sign an `execute` as the swap executor at its reconciled nonce, the shape an early
    /// cancellation takes. Tests record and deliver the payload themselves.
    pub(crate) fn sign_swap_execute_for_tests(
        &self,
        operation: ExecutorOperationId,
        authorization: &DesktopPrivateSpendAuthorization,
        call: &railgun_wallet::TransactionCall,
    ) -> Result<(B256, Bytes)> {
        use alloy::signers::SignerSync as _;
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let (Some(executor), Some(observed)) = (record.address(), record.nonce_observation())
        else {
            return Err(eyre!("swap executor is not reconciled"));
        };
        let context = railgun_wallet::tx::ExecutorContext {
            chain_id: self.chain.chain_id,
            executor,
            delegate: record.delegate(),
            execution_nonce: observed.nonce(),
        };
        let hash = context.signing_hash(call)?;
        let signer = self.authorized_executor_signer(
            authorization,
            &crate::HardwareExecutorAction::Execute(operation),
            operation,
            record.index(),
        )?;
        let signed = context.authorize_call(call, signer.sign_hash_sync(&hash)?)?;
        Ok((hash, signed.data))
    }
}

/// A setup is a paid execute without actions: only the shared execution overhead. Without
/// steps to carry it, the budget adds the chain's gas limit `buffer` once.
fn setup_gas_budget(chain_id: u64, buffer: u64) -> u64 {
    recovery_gas_limits(
        RailgunGasModel::for_chain(chain_id),
        &[],
        ExecutorRecoveryExecution::PaidExecute { nonce: U256::ZERO },
        0,
    )[0]
    .saturating_add(buffer)
}

/// Account code at a canonical block, read only from endpoints admitted for this chain.
async fn code_at(
    endpoints: &ObservationEndpoints,
    address: Address,
    block: BlockNumHash,
) -> Result<Bytes> {
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match trace_step("delegation_rpc", async {
            provider
                .provider
                .get_code_at(address)
                .block_id(BlockId::hash_canonical(block.hash))
                .await
        })
        .instrument(span)
        .await
        {
            Ok(code) => {
                endpoints.succeeded(&provider);
                return Ok(code);
            }
            Err(error) => endpoints.failed(&provider, &error.into()),
        }
    }
    Err(eyre!("executor delegation state is unavailable"))
}
