use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider as _;
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;
use tracing::Instrument as _;

use super::recovery::{PaidExecutionPurpose, recovery_gas_limits};
use super::{
    ExecutorDelivery, ExecutorOwner, ExecutorPaidRecoveryOutcome, ExecutorReconciliationReport,
    ExecutorRecoveryExecution, ExecutorRecoveryFeeEstimate, PreparedExecutorOperation,
};
use crate::desktop::executor_discovery::{execution_nonce_with_code, matches_executor_delegation};
use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::settings::{ExecutorProfile, SwapTokenEligibility};
use crate::vault::{
    ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadPurpose, ExecutorPayloadStatus,
    ExecutorRecord, SwapApproval,
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
pub use bridge::{BridgeLegPrice, SwapBridgeClients, SwapBridgeQuote, SwapBridgeRoute};
pub use observation::{SwapOrderState, swap_order_state};
pub use order::{
    SwapAccountCandidate, SwapAmountPlan, SwapAmountRequest, SwapInputPlan, SwapOrderOutcome,
    SwapOrderRequest, SwapPrice, SwapReview, SwapReviewChange, SwapReviewRequest,
    swap_submission_outcome,
};
#[cfg(test)]
pub(crate) use order::{
    SwapOrderSigning, SwapOutputPoiSink, plan_swap_inputs, price_swap_review, reusable_swap_proof,
    swap_invalidation,
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
    /// Preview a new setup's private fee, or a retry using that operation's own notes.
    pub async fn estimate_swap_setup_fee(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
        candidate: PublicBroadcasterCandidate,
    ) -> Result<ExecutorRecoveryFeeEstimate> {
        self.ensure_active()?;
        self.require_fee_session(session, PaidExecutionPurpose::SwapSetup)?;
        let profile = self.swap_executor_profile()?;
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
    pub fn stop_swap_setup(&self, operation: ExecutorOperationId) -> Result<()> {
        self.ensure_active()?;
        self.swap_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        self.store.stop_swap_setup(operation)?;
        self.notify_change();
        Ok(())
    }

    /// Reserve a fresh executor for a new swap, with the terms the user approved for it. The
    /// record is created holding `approval`, whose pair and delivery bind the first order. The
    /// operation must be unused, so no swap ever takes over another operation's executor.
    /// Setup delivery is broadcaster-only.
    pub async fn prepare_swap_setup(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        approval: SwapApproval,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
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
        self.prepare_operation_with_swap_approval(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            authorization,
            &[ExecutorAsset::Erc20(tokens.sell)],
            Some(SWAP_PURPOSE_SUMMARY),
            Some(approval),
        )
        .await
    }

    /// Retry the setup of an existing swap with the executor it already reserved.
    pub async fn resume_swap_setup(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
        self.swap_executor_profile()?;
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        if record.is_swap_setup_stopped() {
            return Err(eyre!("this swap was stopped"));
        }
        self.prepare_operation(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            authorization,
            record.assets(),
            Some(SWAP_PURPOSE_SUMMARY),
        )
        .await
    }

    /// Pay the selected broadcaster privately for an `execute` with no actions,
    /// carrying only this executor's delegation authorization. The payload is
    /// durable before handoff; completion comes from `observe_swap_setup`.
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
        self.swap_executor_profile()?;
        if self.swap_record(prepared.operation())?.is_none() {
            return Err(eyre!("executor preparation does not belong to a swap"));
        }
        let ExecutorDelivery::PublicBroadcaster(candidate) = prepared.delivery() else {
            return Err(eyre!("swap setup requires a compatible broadcaster"));
        };
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

    fn swap_record(&self, operation: ExecutorOperationId) -> Result<Option<ExecutorRecord>> {
        let Some(record) = self.swap_account_record(operation)? else {
            return Ok(None);
        };
        if !is_swap_record(&record) {
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
