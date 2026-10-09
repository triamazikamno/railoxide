use std::borrow::Borrow;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider as _;
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;
use tracing::Instrument as _;

use super::execution::OperationReservation;
use super::recovery::{PaidExecutionPurpose, recovery_gas_limits, require_private_fee_limit};
use super::{
    ExecutorDelivery, ExecutorOwner, ExecutorPaidRecoveryOutcome, ExecutorReconciliationReport,
    ExecutorRecoveryFeeEstimate, PreparedExecutorOperation,
};
use crate::desktop::executor_discovery::{execution_nonce_with_code, matches_executor_delegation};
use crate::desktop::executor_observation::{
    ObservationEndpoints, read_executor_account, trace_step,
};
use crate::settings::{ExecutorProfile, SwapTokenEligibility};
use crate::vault::{
    BridgeDelivery, ClaimedSwapPair, ExecutorNonceObservation, ExecutorOperationId,
    ExecutorPayloadPurpose, ExecutorRecord, SwapAccountChoice, SwapAccountRole, SwapAccountUse,
    SwapAdmissionEvidence, SwapApproval, SwapApprovalTokens, SwapApprovedAccount,
    SwapApprovedAccounts, SwapDelivery, SwapDestinationClaim, SwapPairClaim, SwapUseCancellation,
    SwapUseId, SwapUseRecord, SwapUseRole, swap_account_refusal,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, HardwareExecutorAction,
    PublicBroadcasterCandidate, TransactionGenerationProgressSender, WakuClient, WalletSession,
};

mod admission;
mod bridge;
mod destination;
mod gas;
mod observation;
mod order;
mod public_order;
mod public_settlement;
mod public_source;
mod public_tracking;
mod public_transactions;
mod recovery;
mod settlement;
mod simulation;
pub use admission::SwapShieldNotes;
#[cfg(test)]
pub(crate) use admission::notes_of_shield;
pub use bridge::{
    BridgeLegPrice, SwapBridgeClients, SwapBridgeQuote, SwapBridgeRoute, SwapPrivateBridgeQuote,
};
pub use observation::{SwapOrderState, swap_order_state};
pub(super) use order::invalidates_order;
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
pub use public_order::{
    PublicSwapBatchTerms, PublicSwapOrderOutcome, PublicSwapOrderRequest, PublicSwapReview,
    PublicSwapReviewRequest, PublicSwapUnavailable, new_public_swap_batch_nonce,
    public_swap_batch_terms,
};
pub use public_settlement::{PublicSwapOrderState, public_swap_order_state};
pub use public_source::{
    PublicSwapDelivery, PublicSwapDeliveryQuote, PublicSwapDeliverySigning, PublicSwapUseClaim,
};
pub use public_tracking::{PublicSwapProgress, PublicSwapTracking};
pub use public_transactions::{
    AuthorizedPublicSwapSource, PUBLIC_ACROSS_DEPOSIT_GAS_UNITS,
    PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS, PUBLIC_PROXY_WITHDRAWAL_GAS_UNITS,
    PublicSwapGasPlan, PublicSwapSource, PublicSwapTransactionOutcome, PublicSwapWithdrawalReview,
    public_swap_approvals, public_swap_gas_plan,
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

/// Whether `record` is the destination stealth account of a private Bridge swap, or of a swap
/// paid from a Public account.
#[must_use]
pub fn is_swap_destination_record(record: &ExecutorRecord) -> bool {
    record.swap_destination().is_some()
        || record
            .swap_uses()
            .last()
            .is_some_and(|swap_use| swap_use.public_swap().is_some())
}

/// Whether the swap use `id` claims `record`'s account and was not stopped.
fn is_live_swap_use(record: &ExecutorRecord, id: SwapUseId) -> bool {
    record.active_swap_use() == Some(id)
        && record
            .swap_use(id)
            .is_some_and(|swap_use| !swap_use.is_stopped())
}

/// Refuse a setup for an account its swap use reuses. Such an account keeps the setup it has,
/// and the use pays no setup fee for it.
fn require_fresh_swap_use(record: &ExecutorRecord) -> Result<()> {
    let reused = record
        .active_swap_use()
        .and_then(|id| record.swap_use(id))
        .is_some_and(|swap_use| !swap_use.is_fresh());
    if reused {
        return Err(eyre!(
            "this swap reuses a stealth account that is already set up; it takes no setup"
        ));
    }
    Ok(())
}

/// Refuse `record`'s account for `swap_use` in `role` unless the shared admission rules pass
/// on `evidence`. The error is the [`crate::vault::SwapAccountRefusal`].
fn require_swap_account(
    record: &ExecutorRecord,
    chain_id: u64,
    role: SwapAccountRole,
    swap_use: SwapAccountUse,
    evidence: SwapAdmissionEvidence,
) -> Result<()> {
    swap_account_refusal(record, chain_id, role, swap_use, evidence)
        .map_or(Ok(()), |refusal| Err(refusal.into()))
}

/// Delivery of an approved setup. The fee ceiling is the reviewed maximum.
/// Borrow the authorization when the same swap needs it again for destination shielding.
pub struct SwapSetupRequest<A = DesktopPrivateSpendAuthorization> {
    pub maximum_private_fee: U256,
    pub session: Arc<WalletSession>,
    pub authorization: A,
    pub waku: Arc<WakuClient>,
    pub verify_proof: bool,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
}

/// The accounts a swap use claims before any of its preparation: see
/// [`ExecutorOwner::claim_swap_use`].
pub struct SwapUseClaim {
    pub id: SwapUseId,
    pub source: SwapAccountChoice,
    /// The approved terms saved with the source use.
    pub approval: SwapApproval,
    /// The destination stealth account, exactly for a private Bridge delivery.
    pub destination: Option<SwapAccountChoice>,
}

/// The accounts one swap use prepares, and what each side needs: see [`prepare_swap_pair`].
pub struct SwapPairPreparation<'a> {
    pub use_id: SwapUseId,
    pub source: SwapAccountChoice,
    /// The destination stealth account, exactly for a private Bridge delivery.
    pub destination: Option<SwapAccountChoice>,
    /// The reviewed terms. A private Bridge delivery's `receiver` is a placeholder until the
    /// destination account's address is known, and `accounts` is bound by the preparation.
    pub approval: SwapApproval,
    /// The source setup's broadcaster, exactly for a new source account.
    pub candidate: Option<PublicBroadcasterCandidate>,
    /// The destination setup's broadcaster, exactly for a new destination account.
    pub destination_candidate: Option<PublicBroadcasterCandidate>,
    pub authorization: &'a DesktopPrivateSpendAuthorization,
    /// Required with a destination account, whether it is new or existing.
    pub destination_authorization: Option<&'a DesktopPrivateSpendAuthorization>,
}

/// One account of a prepared swap pair.
pub enum SwapPairSide {
    /// A new account, reserved and inspected, whose setup is still to submit.
    Setup(PreparedExecutorOperation),
    /// An existing account. It takes no setup and pays no setup fee. Its current state is
    /// checked when the order is signed.
    Existing {
        operation: ExecutorOperationId,
        executor: Address,
    },
}

impl SwapPairSide {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        match self {
            Self::Setup(prepared) => prepared.operation(),
            Self::Existing { operation, .. } => *operation,
        }
    }
    #[must_use]
    pub const fn executor(&self) -> Address {
        match self {
            Self::Setup(prepared) => prepared.context().executor,
            Self::Existing { executor, .. } => *executor,
        }
    }
    /// Whether this side's setup is to submit.
    #[must_use]
    pub const fn requires_setup(&self) -> bool {
        matches!(self, Self::Setup(_))
    }
}

/// The accounts [`prepare_swap_pair`] claimed for one swap use.
pub struct PreparedSwapPair {
    pub use_id: SwapUseId,
    pub origin: SwapPairSide,
    /// `None` when the swap has no destination stealth account.
    pub destination: Option<SwapPairSide>,
    /// The approval saved with the source use. It binds both accounts' addresses and setup
    /// needs, and a private Bridge delivery's `receiver` is the destination account.
    pub approval: SwapApproval,
}

/// Each side's setup result from [`submit_swap_pair_setups`], the origin's first. `None` for a
/// side without a setup: an existing account, or no destination account at all.
pub type SwapPairSetupResults<T = ExecutorPaidRecoveryOutcome> =
    (Option<Result<T>>, Option<Result<T>>);

/// Claim and prepare the accounts of one swap use: the source account on `origin`'s chain and,
/// exactly for a private Bridge delivery, the destination account on `destination`'s chain.
/// Each side is a new account, which the use sets up, or an existing one, which it reuses.
///
/// The request's shape is checked first, and a mismatch reserves nothing. Both accounts are
/// then claimed for the use in one write before either is derived, so neither account ever
/// exists without the swap it serves, and an interrupted preparation keeps both. Repeating the
/// request resumes the same claim. A new account is derived and inspected, the destination's
/// first. An existing account needs the authorization for its own chain and index and takes no
/// setup. It is not read from the chain here: its delegation, nonce and earlier work are
/// checked when the order is signed. The approval saved with the source use then binds both
/// addresses, each side's setup need and the destination account as the delivery's receiver,
/// before anything is returned. A preparation whose use was stopped meanwhile is refused.
///
/// An approved new account that takes its derived address completes the approval. A request
/// that names another account or another setup need than the claim holds is refused.
/// A setup that failed after the pair was prepared is retried alone with
/// [`ExecutorOwner::resume_swap_setup`].
pub async fn prepare_swap_pair(
    origin: &ExecutorOwner,
    destination: Option<&ExecutorOwner>,
    request: SwapPairPreparation<'_>,
) -> Result<PreparedSwapPair> {
    let SwapPairPreparation {
        use_id,
        source,
        destination: destination_account,
        mut approval,
        candidate,
        destination_candidate,
        authorization,
        destination_authorization,
    } = request;
    let delivery = approval.delivery.private_bridge();
    let destination = match (delivery, destination, destination_account) {
        (None, None, None) => None,
        (Some(_), Some(owner), Some(account)) => {
            let authorization = destination_authorization.ok_or_else(|| {
                eyre!("the destination stealth account needs its network's authorization")
            })?;
            Some((owner, account, authorization))
        }
        _ => {
            return Err(eyre!(
                "a destination stealth account belongs to a private Bridge delivery only"
            ));
        }
    };
    let is_new = |account: SwapAccountChoice| matches!(account, SwapAccountChoice::New(_));
    if candidate.is_some() != is_new(source)
        || destination_candidate.is_some()
            != destination.is_some_and(|(_, account, _)| is_new(account))
    {
        return Err(eyre!(
            "a setup broadcaster belongs to a new stealth account only"
        ));
    }
    // An existing account's authorization is checked before anything is claimed.
    let source_address = existing_swap_account(origin, source, authorization)?;
    let destination_address = match destination {
        Some((owner, account, authorization)) => {
            existing_swap_account(owner, account, authorization)?
        }
        None => None,
    };
    approval
        .accounts
        .get_or_insert_with(|| SwapApprovedAccounts {
            source: SwapApprovedAccount {
                address: source_address,
                setup: is_new(source),
            },
            destination: destination.map(|(_, account, _)| SwapApprovedAccount {
                address: destination_address,
                setup: is_new(account),
            }),
        });
    let requested_accounts = approval.accounts;
    let claimed = origin.claim_swap_use(
        destination.map(|(owner, _, _)| owner),
        SwapUseClaim {
            id: use_id,
            source,
            approval: approval.clone(),
            destination: destination.map(|(_, account, _)| account),
        },
    )?;
    // A repeated request resumes its claim, whose accounts keep the setup need they were
    // claimed with.
    let keeps_setup = |record: &ExecutorRecord, account: SwapAccountChoice| {
        record
            .swap_use(use_id)
            .is_some_and(|swap_use| swap_use.is_fresh() == is_new(account))
    };
    if !keeps_setup(&claimed.source, source)
        || destination.is_some_and(|(_, account, _)| {
            !claimed
                .destination
                .as_ref()
                .is_some_and(|record| keeps_setup(record, account))
        })
    {
        return Err(eyre!(ACCOUNTS_CHANGED));
    }
    let approved = claimed
        .source
        .swap_use(use_id)
        .and_then(SwapUseRecord::approval)
        .and_then(|approval| approval.accounts);
    let prepared_destination = match (destination, destination_candidate) {
        (Some((owner, SwapAccountChoice::New(operation), authorization)), Some(candidate)) => {
            Some(SwapPairSide::Setup(
                Box::pin(owner.resume_swap_setup(operation, candidate, authorization)).await?,
            ))
        }
        (Some((_, account, _)), _) => Some(SwapPairSide::Existing {
            operation: account.operation(),
            executor: destination_address.ok_or_else(|| eyre!("stealth account is unavailable"))?,
        }),
        (None, _) => None,
    };
    let prepared_origin = match (source, candidate) {
        (SwapAccountChoice::New(operation), Some(candidate)) => SwapPairSide::Setup(
            Box::pin(origin.resume_swap_setup(operation, candidate, authorization)).await?,
        ),
        _ => SwapPairSide::Existing {
            operation: source.operation(),
            executor: source_address.ok_or_else(|| eyre!("stealth account is unavailable"))?,
        },
    };
    // The store takes an approval only for a derived account, so the addresses are bound once
    // the swap's own account is derived, before either preparation can issue a payload.
    let bound_source = (prepared_origin.executor(), prepared_origin.requires_setup());
    let bound_destination = prepared_destination
        .as_ref()
        .map(|side| (side.executor(), side.requires_setup()));
    if requested_accounts.is_some_and(|approved| !approved.admits(bound_source, bound_destination))
        || approved.is_some_and(|approved| !approved.admits(bound_source, bound_destination))
    {
        return Err(eyre!(ACCOUNTS_CHANGED));
    }
    let bind = |(address, setup): (Address, bool)| SwapApprovedAccount {
        address: Some(address),
        setup,
    };
    approval.accounts = Some(SwapApprovedAccounts {
        source: bind(bound_source),
        destination: bound_destination.map(bind),
    });
    if let (Some(delivery), Some((receiver, _))) = (delivery, bound_destination) {
        approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
            receiver,
            ..delivery
        });
    }
    // An existing source may have served as a destination before, so its record is not
    // required to be a swap's own.
    origin.ensure_active()?;
    origin
        .store
        .record_swap_approval(source.operation(), use_id, approval.clone())?;
    origin.notify_change();
    origin.require_live_swap_use(source.operation(), Some(use_id))?;
    if let Some((owner, account, _)) = destination {
        owner.require_live_swap_use(account.operation(), Some(use_id))?;
    }
    Ok(PreparedSwapPair {
        use_id,
        origin: prepared_origin,
        destination: prepared_destination,
        approval,
    })
}

const ACCOUNTS_CHANGED: &str =
    "the swap's stealth accounts differ from the ones it was prepared with; review the swap again";

/// The recorded address of an existing account of a swap pair, after requiring the
/// authorization for its chain and index. `None` for a new account, which has no record yet.
fn existing_swap_account(
    owner: &ExecutorOwner,
    account: SwapAccountChoice,
    authorization: &DesktopPrivateSpendAuthorization,
) -> Result<Option<Address>> {
    let SwapAccountChoice::Existing(operation) = account else {
        return Ok(None);
    };
    owner.require_executor_authorization(
        authorization,
        &HardwareExecutorAction::Execute(operation),
    )?;
    owner
        .swap_account_record(operation)?
        .and_then(|record| record.address())
        .map(Some)
        .ok_or_else(|| eyre!("stealth account is unavailable"))
}

/// Submit the setups a prepared pair needs, each through its own owner and session: both at
/// the same time when both accounts are new, one when one is, and none when both exist. Each
/// result stands by itself. A failed setup is retried alone with
/// [`ExecutorOwner::resume_swap_setup`], without repeating the other, and an existing account
/// is never set up or charged: its request, if any, is dropped unused.
pub async fn submit_swap_pair_setups(
    origin: (&ExecutorOwner, &SwapPairSide, Option<SwapSetupRequest>),
    destination: Option<(&ExecutorOwner, &SwapPairSide, Option<SwapSetupRequest>)>,
) -> SwapPairSetupResults {
    submit_swap_pair_setups_with(origin, destination, |owner, prepared, request| {
        Box::pin(owner.submit_swap_setup(prepared, request))
    })
    .await
}

/// [`submit_swap_pair_setups`] over `submit`, which hands one new account's setup off.
pub(crate) async fn submit_swap_pair_setups_with<'a, R, T, F, S>(
    origin: (&'a ExecutorOwner, &'a SwapPairSide, Option<R>),
    destination: Option<(&'a ExecutorOwner, &'a SwapPairSide, Option<R>)>,
    submit: F,
) -> SwapPairSetupResults<T>
where
    F: Fn(&'a ExecutorOwner, &'a PreparedExecutorOperation, R) -> S,
    S: Future<Output = Result<T>>,
{
    let side = move |(owner, side, request): (&'a ExecutorOwner, &'a SwapPairSide, Option<R>)| {
        let setup = match side {
            SwapPairSide::Setup(prepared) => {
                Some(request.map(|request| submit(owner, prepared, request)))
            }
            SwapPairSide::Existing { .. } => None,
        };
        async move {
            match setup? {
                Some(submitting) => Some(submitting.await),
                None => Some(Err(eyre!("this setup has no approved fee limit"))),
            }
        }
    };
    let (origin, destination) = (side(origin), destination.map(side));
    tokio::join!(origin, async {
        match destination {
            Some(destination) => destination.await,
            None => None,
        }
    })
}

/// A swap executor that canonically carries the accepted delegation, with an execution
/// nonce past its setup's. Order preparation for the swap starts here.
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
    /// The latest issued setup whose nonce is resolved, recorded in the swap's terms with
    /// its first order. Which setup signed at that nonce ran is not established.
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
    /// No setup's nonce is consumed at the confirmed block yet. A setup that reverted or
    /// whose authorization was skipped leaves its nonce unconsumed and stays here.
    Pending,
    /// A setup's nonce is consumed and the account lacks the accepted delegation.
    MissingDelegation,
    Delegated(DelegatedSwapExecutor),
}

/// Evaluate a reconciled swap record against the account code read at its nonce
/// observation block. Delegation needs the accepted designator and an execution nonce
/// past a setup's nonce; neither signal suffices alone. Which of several setups signed at
/// that nonce ran is not asked.
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
    let Some(setup) = resolved_swap_setup(record) else {
        return SwapSetupStatus::Pending;
    };
    if !matches_executor_delegation(code, profile) {
        return SwapSetupStatus::MissingDelegation;
    }
    SwapSetupStatus::Delegated(DelegatedSwapExecutor {
        operation: record.operation(),
        executor,
        delegate: record.delegate(),
        setup_payload: setup,
        observed,
    })
}

/// The latest issued setup whose nonce is resolved. Several setups signed at one nonce,
/// as fee re-quotes and retries leave, are one outcome.
fn resolved_swap_setup(record: &ExecutorRecord) -> Option<B256> {
    record
        .issued()
        .iter()
        .rev()
        .find(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::Operation
                && record.payload_state(payload.hash())
                    == Some(crate::vault::ExecutorPayloadState::Resolved)
        })
        .map(crate::vault::IssuedExecutorPayload::hash)
}

/// Whether a swap without orders records a resolved setup for `profile`'s delegate: a setup
/// nonce the watermark shows consumed, whichever setup signed at it ran. This is recorded
/// progress under the wallet's issuance assumptions. A consumed nonce says nothing about
/// the delegation designator, so this never authorizes signing; order preparation checks
/// the account afresh with [`swap_setup_status`].
#[must_use]
pub fn swap_setup_recorded_executed(record: &ExecutorRecord, profile: ExecutorProfile) -> bool {
    record.swap().is_none()
        && record.delegate() == profile.delegate()
        && record.issued().iter().any(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::Operation
                && record.nonce_resolved(payload.nonce())
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
    ///
    /// The account is a swap's own, or one a swap use claims as its source: an account that
    /// was a destination before is not a swap record, and its source use takes the approval.
    /// The store still refuses a stopped use and a use that has an order.
    pub fn record_swap_approval(
        &self,
        operation: ExecutorOperationId,
        expected_use: SwapUseId,
        approval: SwapApproval,
    ) -> Result<()> {
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let claimed_as_source = record
            .active_swap_use()
            .and_then(|id| record.swap_use(id))
            .is_some_and(|swap_use| matches!(swap_use.role(), SwapUseRole::Source { .. }));
        if !is_swap_record(&record) && !claimed_as_source {
            return Err(eyre!("this executor does not belong to a swap"));
        }
        self.store
            .record_swap_approval(operation, expected_use, approval)?;
        self.notify_change();
        Ok(())
    }

    /// Claim the accounts of one swap use together, before any of its preparation: this
    /// chain's source account and, exactly for a private Bridge delivery, the destination
    /// account on `destination`'s chain. Both are claimed or neither, and a claim both accounts
    /// already hold is returned unchanged. It reads no chain state and derives no key.
    pub fn claim_swap_use(
        &self,
        destination: Option<&Self>,
        request: SwapUseClaim,
    ) -> Result<ClaimedSwapPair> {
        let SwapUseClaim {
            id,
            source,
            approval,
            destination: destination_account,
        } = request;
        self.ensure_active()?;
        let delegate = self.swap_executor_profile()?.delegate();
        let tokens = self.validated_swap_tokens(&approval)?;
        let private = approval.delivery.private_bridge();
        // A new account's setup is paid within the limit approved for its chain.
        if matches!(source, SwapAccountChoice::New(_)) && approval.bounds.source_setup_fee.is_none()
        {
            return Err(eyre!("the swap's approval has no setup fee limit"));
        }
        if matches!(destination_account, Some(SwapAccountChoice::New(_)))
            && approval.bounds.destination_setup_fee.is_none()
        {
            return Err(eyre!(
                "the swap's approval has no destination setup fee limit"
            ));
        }
        let destination = match (private, destination, destination_account) {
            (None, None, None) => None,
            (Some(delivery), Some(owner), Some(account)) => {
                if !delivery.has_valid_private_delivery()
                    || delivery.destination_chain != owner.chain.chain_id
                    || delivery.destination_chain == self.chain.chain_id
                {
                    return Err(eyre!(
                        "the swap's approval is not a private Bridge delivery to this destination network"
                    ));
                }
                if !self.view.is_same_wallet_session(&owner.view) {
                    return Err(eyre!(
                        "the destination network belongs to another wallet session"
                    ));
                }
                owner.ensure_active()?;
                let claim = SwapDestinationClaim {
                    chain_id: delivery.destination_chain,
                    account,
                    delegate: owner.swap_destination_profile()?.delegate(),
                    destination_token: delivery.destination_token,
                };
                Some((owner, claim))
            }
            _ => {
                return Err(eyre!(
                    "a destination stealth account belongs to a private Bridge delivery only"
                ));
            }
        };
        // Recovery inspects the sell token. `record_swap_attempt` adds the buy token for a
        // Reshield order, the only kind that pays it to the executor.
        let claimed = self.store.claim_swap_pair(SwapPairClaim {
            id,
            source,
            delegate,
            purpose_summary: Some(SWAP_PURPOSE_SUMMARY.to_owned()),
            assets: vec![ExecutorAsset::Erc20(tokens.sell)],
            approval,
            destination: destination.as_ref().map(|(_, claim)| claim.clone()),
        })?;
        self.notify_change();
        if let Some((owner, _)) = destination {
            owner.notify_change();
        }
        Ok(claimed)
    }

    /// Cancel a prepared swap use that has no order yet, through this source chain's owner.
    /// Further setup and signing for the use stop on both accounts, and each account is
    /// released or kept as the result reports. An existing account is never retired by this.
    pub fn cancel_swap_use(
        &self,
        destination: Option<&Self>,
        operation: ExecutorOperationId,
        id: SwapUseId,
    ) -> Result<SwapUseCancellation> {
        self.ensure_active()?;
        if let Some(destination) = destination {
            destination.ensure_active()?;
            if !self.view.is_same_wallet_session(&destination.view) {
                return Err(eyre!(
                    "the destination network belongs to another wallet session"
                ));
            }
        }
        let cancellation = self.store.cancel_swap_use(operation, id)?;
        self.notify_change();
        if let Some(destination) = destination {
            destination.notify_change();
        }
        Ok(cancellation)
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
        let private_bridge = approval.delivery.private_bridge().is_some();
        if private_bridge != destination_operation.is_some() {
            return Err(eyre!(
                "a destination stealth account belongs to a private Bridge delivery only"
            ));
        }
        let tokens = self.validated_swap_tokens(&approval)?;
        if self.swap_record(operation)?.is_some() {
            return Err(eyre!(
                "this swap already has an executor; resume it instead"
            ));
        }
        // Recovery inspects the sell token. `record_swap_attempt` adds the buy token for a
        // Reshield order, the only kind that pays it to the executor.
        let prepared = self
            .prepare_reserved_operation(
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
            .await?;
        self.require_live_swap_use(operation, Some(SwapUseId::first(operation)))?;
        Ok(prepared)
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
        let swap_use = record.active_swap_use();
        if record.is_swap_setup_stopped()
            || swap_use.is_some_and(|id| !is_live_swap_use(&record, id))
        {
            return Err(eyre!("this swap was stopped"));
        }
        require_fresh_swap_use(&record)?;
        // The reservation is returned only for the links its record was created with.
        let reservation = match record.swap_destination() {
            Some(destination) => OperationReservation::SwapDestination(destination),
            None => OperationReservation::Operation {
                purpose_summary: Some(SWAP_PURPOSE_SUMMARY),
                swap_approval: None,
                destination_operation: record.destination_operation(),
            },
        };
        let prepared = self
            .prepare_reserved_operation(
                operation,
                ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
                authorization,
                record.assets(),
                reservation,
            )
            .await?;
        self.require_live_swap_use(operation, swap_use)?;
        Ok(prepared)
    }

    /// Refuse a preparation whose swap use was stopped or replaced while its account was
    /// inspected. `swap_use` is the use the preparation started for; an account without one
    /// has nothing to check.
    fn require_live_swap_use(
        &self,
        operation: ExecutorOperationId,
        swap_use: Option<SwapUseId>,
    ) -> Result<()> {
        let Some(id) = swap_use else {
            return Ok(());
        };
        let live = self
            .swap_account_record(operation)?
            .is_some_and(|record| is_live_swap_use(&record, id));
        if live {
            Ok(())
        } else {
            Err(eyre!("this swap was stopped"))
        }
    }

    /// Pay the selected broadcaster privately for an `execute` with no actions,
    /// carrying only this executor's delegation authorization. The payload is
    /// durable before handoff; completion comes from `observe_swap_setup`. A destination
    /// stealth account's fee ceiling may not exceed the one approved with its swap, and neither
    /// may the swap's own when its approval binds one. An account its swap reuses takes no
    /// setup.
    pub async fn submit_swap_setup<A: Borrow<DesktopPrivateSpendAuthorization> + Send>(
        &self,
        prepared: &PreparedExecutorOperation,
        request: SwapSetupRequest<A>,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        self.while_active(Box::pin(self.submit_swap_setup_active(prepared, request)))
            .await
    }

    async fn submit_swap_setup_active<A: Borrow<DesktopPrivateSpendAuthorization> + Send>(
        &self,
        prepared: &PreparedExecutorOperation,
        request: SwapSetupRequest<A>,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        self.require_fee_session(&request.session, PaidExecutionPurpose::SwapSetup)?;
        let record = self
            .swap_setup_record(prepared.operation())?
            .ok_or_else(|| eyre!("executor preparation does not belong to a swap"))?;
        self.swap_setup_profile(Some(&record))?;
        let ExecutorDelivery::PublicBroadcaster(candidate) = prepared.delivery() else {
            return Err(eyre!("swap setup requires a compatible broadcaster"));
        };
        require_fresh_swap_use(&record)?;
        if is_swap_destination_record(&record) {
            self.require_swap_destination_setup_fee(
                record.operation(),
                candidate.token,
                request.maximum_private_fee,
            )?;
        } else {
            self.require_swap_source_setup_fee(
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

    /// One pass of a setup wait: read the executor's code and execution nonce at the
    /// confirmed tip, record the nonce, and decide from that state alone. No block, receipt
    /// or transaction is read, so a pass costs the same however long ago the setup was
    /// signed. A retry of a delegated swap resumes from the returned executor.
    ///
    /// The read is a fact about the chain, not a decision taken from a snapshot of the
    /// record, so it is applied to the record as it is by then. Another observation
    /// writing during the read leaves the setup pending at worst, and the next pass reads
    /// again.
    pub async fn observe_swap_setup(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<SwapSetupStatus> {
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let Some(executor) = record.address() else {
            return Ok(SwapSetupStatus::Pending);
        };
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("this swap executor's delegate is not supported"))?;
        let mut chain = self
            .chain_for_delegate(record.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
        chain.enabled = true;
        let read = trace_step(
            "setup_account",
            self.while_active(read_executor_account(
                &self.endpoints,
                &chain,
                executor,
                None,
            )),
        )
        .await?;
        // Code the execution nonce is not read under shows nothing about the setup's nonce.
        let Some(observed) = read.nonce else {
            return Ok(SwapSetupStatus::Pending);
        };
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self.apply_account_read(operation, observed)?;
        Ok(swap_setup_status(&record, read.block, &read.code, profile))
    }

    /// Confirm this chain's destination stealth account of a private Bridge swap at
    /// `confirmed`, before the swap's order is signed on `origin_chain`. The account must be
    /// delegated there, be claimed by the swap use `swap_use` as the destination of the swap
    /// `origin_operation`, and be `receiver`, the account the order's delivery names. Nothing
    /// is signed.
    ///
    /// An account the use reserved fresh is confirmed from its setup by one account read at
    /// the confirmed tip. An account the use
    /// reuses passes [`Self::reuse_swap_account`] as a destination, and `notes`, the wallet's
    /// local notes on this chain, must show no earlier shield with an open POI verdict or
    /// refund. Its receiving-token balance is read when the shield is issued.
    pub(crate) async fn delegated_swap_destination(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
        origin_chain: u64,
        origin_operation: ExecutorOperationId,
        swap_use: SwapUseId,
        receiver: Address,
        notes: Option<&dyn SwapShieldNotes>,
    ) -> Result<DelegatedSwapExecutor> {
        self.delegated_destination(operation, confirmed, swap_use, notes, |record| {
            record.serves_swap_use(swap_use, origin_chain, origin_operation)
                && record.address() == Some(receiver)
        })
        .await
    }

    /// Confirm at `confirmed` the destination account `operation` that the live swap use
    /// `swap_use` claims, as [`Self::delegated_swap_destination`] describes. `claimed` tells
    /// whether a record is the account that use delivers to.
    async fn delegated_destination(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
        swap_use: SwapUseId,
        notes: Option<&dyn SwapShieldNotes>,
        claimed: impl Fn(&ExecutorRecord) -> bool,
    ) -> Result<DelegatedSwapExecutor> {
        let serves = |record: &ExecutorRecord| {
            is_live_swap_use(record, swap_use)
                && claimed(record)
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
            // The token a use that didn't reserve this account fresh receives here.
            let reused = record
                .swap_use(swap_use)
                .filter(|claimed| !claimed.is_fresh())
                .and_then(|claimed| match claimed.role() {
                    SwapUseRole::Destination {
                        destination_token, ..
                    }
                    | SwapUseRole::PublicSourceDestination {
                        destination_token, ..
                    } => Some(*destination_token),
                    SwapUseRole::Source { .. } => None,
                });
            if let Some(token) = reused {
                let executor = Box::pin(self.reuse_swap_account(
                    operation,
                    confirmed,
                    SwapAccountRole::Destination { token },
                    SwapAccountUse::Claimed(swap_use),
                ))
                .await?;
                let delegated = executor.delegated().ok_or_else(unavailable)?;
                let record = self.swap_account_record(operation)?.ok_or_else(unavailable)?;
                if !serves(&record) {
                    return Err(unavailable());
                }
                admission::require_earlier_shields_resolved(&record, swap_use, notes)?;
                return Ok(delegated);
            }
            let report =
                trace_step("destination_account", self.reconcile_account(operation)).await?;
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
        // Only a setup whose nonce is resolved depends on the account's code.
        let resolved = resolved_swap_setup(record).is_some();
        // The account read loaded the code; reuse it when the record's nonce is from that block.
        let code = if !resolved {
            Bytes::new()
        } else if report.code.0 == observed.block() {
            report.code.1.clone()
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
    /// with its swap. A swap paid from a Public account saves it with the use that claims this
    /// account, and any other in the swap's record on its own chain.
    pub(crate) fn require_swap_destination_setup_fee(
        &self,
        operation: ExecutorOperationId,
        fee_token: Address,
        maximum_private_fee: U256,
    ) -> Result<()> {
        self.ensure_active()?;
        let mut approved = self.store.public_swap_destination_setup_fee(operation)?;
        if approved.is_none() {
            approved = self
                .store
                .swap_destination_origin(operation)?
                .and_then(|origin| origin.swap_approval()?.bounds.destination_setup_fee);
        }
        let approved = approved
            .ok_or_else(|| eyre!("this destination stealth account has no approved setup fee"))?;
        require_private_fee_limit(
            fee_token,
            maximum_private_fee,
            approved,
            PaidExecutionPurpose::SwapSetup,
        )
    }

    /// Refuse a swap's own setup's private fee ceiling above the source setup fee approved with
    /// the swap. An approval from before that fee was bound has none and refuses nothing.
    pub(crate) fn require_swap_source_setup_fee(
        &self,
        operation: ExecutorOperationId,
        fee_token: Address,
        maximum_private_fee: U256,
    ) -> Result<()> {
        let approved = self
            .swap_account_record(operation)?
            .and_then(|record| record.swap_approval()?.bounds.source_setup_fee);
        if let Some(approved) = approved {
            require_private_fee_limit(
                fee_token,
                maximum_private_fee,
                approved,
                PaidExecutionPurpose::SwapSetup,
            )?;
        }
        Ok(())
    }

    fn validated_swap_tokens(&self, approval: &SwapApproval) -> Result<SwapApprovalTokens> {
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
        Ok(tokens)
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

    /// User-selected reuse needs current account state, not another swap observation, when all
    /// swaps already have finalized delivery, whichever role the account had in them. Reads
    /// are outside activity; commit checks that no signing, recovery or observation changed
    /// the snapshot meanwhile, and then judges the refreshed record for `swap_use` in `role`.
    async fn refresh_settled_swap(
        &self,
        previous: &ExecutorRecord,
        role: SwapAccountRole,
        swap_use: SwapAccountUse,
    ) -> Result<SwapExecutor> {
        let executor = previous
            .address()
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let chain = self
            .chain_for_delegate(previous.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
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
                    require_swap_account(
                        &record,
                        self.chain.chain_id,
                        role,
                        swap_use,
                        SwapAdmissionEvidence::Fresh,
                    )?;
                    // Use the retained setup and fresh nonce; nothing is attributed.
                    let setup = resolved_swap_setup(&record)
                        .ok_or_else(|| eyre!("the account's setup is unavailable"))?;
                    return Ok(SwapExecutor::from(DelegatedSwapExecutor {
                        operation: record.operation(),
                        executor,
                        delegate: record.delegate(),
                        setup_payload: setup,
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
    recovery_gas_limits(RailgunGasModel::for_chain(chain_id), &[], 0)[0].saturating_add(buffer)
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
