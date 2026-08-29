//! Typed, context-bound public transactions for Governance.
//!
//! This module owns the protocol-facing draft boundary.  It deliberately does not own a signer,
//! nonce manager, RPC client, or receipt watcher: those remain in `public_wallet`.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};
use thiserror::Error;

use crate::governance::GovernanceContractVersion;
use crate::http::HttpContext;
use crate::public_wallet::{
    PublicActionFeeProjection, PublicActionGasFeeSelection, PublicActionProgressStep,
    PublicAdvancedTransactionAuthorization, PublicAdvancedTransactionEstimate,
    PublicAdvancedTransactionEstimateRequest, PublicAdvancedTransactionSimulationError,
    PublicSendRequest, PublicSendResult, PublicTransactionIntent, VaultedPublicSigner,
    estimate_public_advanced_transaction_with_fee, simulate_public_advanced_transaction_with_fee,
    submit_public_action_step_with_signer, vaulted_public_signer,
};
use crate::settings::EffectiveChainConfig;
use crate::{
    DEFAULT_MULTICALL_CHUNK_SIZE, MulticallChunkSize, StakePlan, UndelegateFirstPlan,
    continue_stake_after_approval, continue_unlock_after_undelegation, fetch_account_stakes,
    fetch_governance_token_balance_allowance, fetch_staking_global_metrics,
};
use railgun_ui::governance_contracts;

sol! {
    interface GovernanceVotingActions {
        function sponsorProposal(uint256 proposalID, uint256 amount, address account, uint256 hint) external;
        function unsponsorProposal(uint256 proposalID, uint256 amount, address account) external;
        function callVote(uint256 proposalID) external;
        function vote(uint256 proposalID, uint256 amount, bool yay, address account, uint256 hint) external;
    }

    interface GovernanceStakingActions {
        function stake(uint256 amount) external;
        function delegate(uint256 stakeId, address delegatee) external;
        function undelegate(uint256 stakeId) external;
        function unlock(uint256 stakeId) external;
        function claim(uint256 stakeId) external;
    }

    interface GovernanceRewardsActions {
        function claim(address[] tokens, address account, uint256 startingInterval, uint256 endingInterval, uint256[] hints) external;
    }

    interface GovernanceErc20Actions {
        function approve(address spender, uint256 amount) external;
    }
}

/// Every write exposed by Governance.  There is intentionally no proposal execution variant:
/// arbitrary proposal actions are inspection data, not wallet-owned transaction intents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GovernanceActionIntent {
    Sponsor {
        proposal_version: GovernanceContractVersion,
        proposal_index: U256,
        amount: U256,
        snapshot_interval: U256,
        snapshot_hint: U256,
    },
    Unsponsor {
        proposal_version: GovernanceContractVersion,
        proposal_index: U256,
        amount: U256,
    },
    CallVote {
        proposal_version: GovernanceContractVersion,
        proposal_index: U256,
    },
    Yay {
        proposal_version: GovernanceContractVersion,
        proposal_index: U256,
        amount: U256,
        snapshot_interval: U256,
        snapshot_hint: U256,
    },
    Nay {
        proposal_version: GovernanceContractVersion,
        proposal_index: U256,
        amount: U256,
        snapshot_interval: U256,
        snapshot_hint: U256,
    },
    GovernanceTokenApproval {
        spender: Address,
        amount: U256,
    },
    Stake {
        amount: U256,
    },
    Delegate {
        stake_id: U256,
        delegate: Address,
    },
    Undelegate {
        stake_id: U256,
    },
    Unlock {
        stake_id: U256,
    },
    PrincipalClaim {
        stake_id: U256,
    },
    RewardClaim {
        reward_tokens: Vec<Address>,
        starting_interval: U256,
        ending_interval: U256,
        snapshot_hints: Vec<U256>,
        expected_amounts: Vec<U256>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceContractKind {
    Voting,
    GovernanceToken,
    Staking,
    GovernorRewards,
}

/// Immutable identity and state observed while preparing an action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceActionContext {
    pub private_wallet_uuid: String,
    pub chain_id: u64,
    pub public_account_uuid: String,
    pub actor: Address,
    pub contract: Address,
    pub contract_kind: GovernanceContractKind,
    /// Hash of the relevant fresh state (proposal/account/stake/reward evidence).
    pub observed_state: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceResolvedAction {
    pub intent: GovernanceActionIntent,
    pub raw: PublicTransactionIntent,
    pub fingerprint: B256,
}

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum GovernanceActionError {
    #[error("governance action amount must be positive")]
    NonPositiveAmount,
    #[error("governance-token approval must be amount-bounded")]
    UnboundedApproval,
    #[error("governance reward interval range is invalid")]
    InvalidRewardRange,
    #[error("governance reward snapshot hints do not match the interval range")]
    InvalidRewardHints,
    #[error("governance reward tokens and expected amounts must have matching lengths")]
    InvalidRewardAmounts,
    #[error("governance reward token addresses must be strictly ascending and unique")]
    InvalidRewardTokenOrder,
    #[error("governance reward claim must include a positive expected amount")]
    ZeroReward,
    #[error("governance action draft changed before authorization")]
    StaleDraft,
    #[error("governance action requires an advanced public transaction authorization")]
    MissingAuthorization,
    #[error("governance action requires a current transaction estimate")]
    MissingEstimate,
    #[error("governance authorization does not match the current estimate")]
    MismatchedAuthorization,
    #[error("governance action is bound to a different public account or chain")]
    WrongPublicContext,
    #[error("governance action delegate cannot be the zero address")]
    ZeroAddress,
    #[error("governance action targets the wrong governance contract class")]
    WrongContractKind,
    #[error("governance workflow does not match its initial exact call")]
    InvalidWorkflow,
    #[error("fresh governance workflow state is unavailable")]
    FreshStateUnavailable,
    #[error("governance simulation failed: {0}")]
    Simulation(String),
}

impl GovernanceActionContext {
    /// Hash the immutable context, exact raw call, and typed action parameters.
    #[must_use]
    pub fn fingerprint(
        &self,
        intent: &GovernanceActionIntent,
        raw: &PublicTransactionIntent,
    ) -> B256 {
        let mut encoded = Vec::with_capacity(256);
        append_bytes(&mut encoded, b"railoxide:governance-action:v1");
        append_bytes(&mut encoded, self.private_wallet_uuid.as_bytes());
        encoded.extend_from_slice(&self.chain_id.to_be_bytes());
        append_bytes(&mut encoded, self.public_account_uuid.as_bytes());
        encoded.extend_from_slice(self.actor.as_slice());
        encoded.extend_from_slice(self.contract.as_slice());
        encoded.push(match self.contract_kind {
            GovernanceContractKind::Voting => 0,
            GovernanceContractKind::GovernanceToken => 1,
            GovernanceContractKind::Staking => 2,
            GovernanceContractKind::GovernorRewards => 3,
        });
        encoded.extend_from_slice(self.observed_state.as_slice());
        append_intent(&mut encoded, intent);
        append_raw(&mut encoded, raw);
        keccak256(encoded)
    }
}

impl GovernanceActionIntent {
    /// Resolve a typed action to exactly one zero-value contract call.
    pub fn resolve(
        &self,
        context: &GovernanceActionContext,
    ) -> std::result::Result<GovernanceResolvedAction, GovernanceActionError> {
        if !self.matches_contract_kind(context.contract_kind) {
            return Err(GovernanceActionError::WrongContractKind);
        }
        let data = match self {
            Self::Sponsor {
                proposal_index,
                amount,
                snapshot_hint,
                ..
            } => {
                positive(*amount)?;
                GovernanceVotingActions::sponsorProposalCall {
                    proposalID: *proposal_index,
                    amount: *amount,
                    account: context.actor,
                    hint: *snapshot_hint,
                }
                .abi_encode()
            }
            Self::Unsponsor {
                proposal_index,
                amount,
                ..
            } => {
                positive(*amount)?;
                GovernanceVotingActions::unsponsorProposalCall {
                    proposalID: *proposal_index,
                    amount: *amount,
                    account: context.actor,
                }
                .abi_encode()
            }
            Self::CallVote { proposal_index, .. } => GovernanceVotingActions::callVoteCall {
                proposalID: *proposal_index,
            }
            .abi_encode(),
            Self::Yay {
                proposal_index,
                amount,
                snapshot_hint,
                ..
            } => {
                positive(*amount)?;
                GovernanceVotingActions::voteCall {
                    proposalID: *proposal_index,
                    amount: *amount,
                    yay: true,
                    account: context.actor,
                    hint: *snapshot_hint,
                }
                .abi_encode()
            }
            Self::Nay {
                proposal_index,
                amount,
                snapshot_hint,
                ..
            } => {
                positive(*amount)?;
                GovernanceVotingActions::voteCall {
                    proposalID: *proposal_index,
                    amount: *amount,
                    yay: false,
                    account: context.actor,
                    hint: *snapshot_hint,
                }
                .abi_encode()
            }
            Self::GovernanceTokenApproval { spender, amount } => {
                positive(*amount)?;
                if *amount == U256::MAX {
                    return Err(GovernanceActionError::UnboundedApproval);
                }
                GovernanceErc20Actions::approveCall {
                    spender: *spender,
                    amount: *amount,
                }
                .abi_encode()
            }
            Self::Stake { amount } => {
                positive(*amount)?;
                GovernanceStakingActions::stakeCall { amount: *amount }.abi_encode()
            }
            Self::Delegate { stake_id, delegate } => {
                if *delegate == Address::ZERO {
                    return Err(GovernanceActionError::ZeroAddress);
                }
                GovernanceStakingActions::delegateCall {
                    stakeId: *stake_id,
                    delegatee: *delegate,
                }
                .abi_encode()
            }
            Self::Undelegate { stake_id } => {
                GovernanceStakingActions::undelegateCall { stakeId: *stake_id }.abi_encode()
            }
            Self::Unlock { stake_id } => {
                GovernanceStakingActions::unlockCall { stakeId: *stake_id }.abi_encode()
            }
            Self::PrincipalClaim { stake_id } => {
                GovernanceStakingActions::claimCall { stakeId: *stake_id }.abi_encode()
            }
            Self::RewardClaim {
                reward_tokens,
                starting_interval,
                ending_interval,
                snapshot_hints,
                expected_amounts,
            } => {
                if reward_tokens.is_empty() || reward_tokens.len() != expected_amounts.len() {
                    return Err(GovernanceActionError::InvalidRewardAmounts);
                }
                if reward_tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err(GovernanceActionError::InvalidRewardTokenOrder);
                }
                if expected_amounts.iter().all(U256::is_zero) {
                    return Err(GovernanceActionError::ZeroReward);
                }
                if starting_interval > ending_interval {
                    return Err(GovernanceActionError::InvalidRewardRange);
                }
                let expected = ending_interval
                    .checked_sub(*starting_interval)
                    .and_then(|n| n.checked_add(U256::from(1_u8)))
                    .ok_or(GovernanceActionError::InvalidRewardRange)?;
                if U256::from(snapshot_hints.len()) != expected {
                    return Err(GovernanceActionError::InvalidRewardHints);
                }
                GovernanceRewardsActions::claimCall {
                    tokens: reward_tokens.clone(),
                    account: context.actor,
                    startingInterval: *starting_interval,
                    endingInterval: *ending_interval,
                    hints: snapshot_hints.clone(),
                }
                .abi_encode()
            }
        };
        let raw = PublicTransactionIntent::Raw {
            to: Some(context.contract),
            value: U256::ZERO,
            data: data.into(),
        };
        let fingerprint = context.fingerprint(self, &raw);
        Ok(GovernanceResolvedAction {
            intent: self.clone(),
            raw,
            fingerprint,
        })
    }

    #[must_use]
    pub const fn matches_contract_kind(&self, kind: GovernanceContractKind) -> bool {
        matches!(
            (self, kind),
            (
                Self::Sponsor { .. }
                    | Self::Unsponsor { .. }
                    | Self::CallVote { .. }
                    | Self::Yay { .. }
                    | Self::Nay { .. },
                GovernanceContractKind::Voting
            ) | (
                Self::GovernanceTokenApproval { .. },
                GovernanceContractKind::GovernanceToken
            ) | (
                Self::Stake { .. }
                    | Self::Delegate { .. }
                    | Self::Undelegate { .. }
                    | Self::Unlock { .. }
                    | Self::PrincipalClaim { .. },
                GovernanceContractKind::Staking
            ) | (
                Self::RewardClaim { .. },
                GovernanceContractKind::GovernorRewards
            )
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GovernanceActionReview {
    pub context: GovernanceActionContext,
    pub action: GovernanceActionIntent,
    pub calldata: Bytes,
    pub native_value: U256,
    pub amount: Option<U256>,
    pub estimated_fee: Option<PublicActionFeeProjection>,
    pub fingerprint: B256,
}

impl GovernanceActionReview {
    pub fn from_resolved(
        resolved: &GovernanceResolvedAction,
        context: GovernanceActionContext,
        estimated_fee: Option<PublicActionFeeProjection>,
    ) -> std::result::Result<Self, GovernanceActionError> {
        if context.fingerprint(&resolved.intent, &resolved.raw) != resolved.fingerprint {
            return Err(GovernanceActionError::StaleDraft);
        }
        let PublicTransactionIntent::Raw { value, data, .. } = &resolved.raw else {
            return Err(GovernanceActionError::StaleDraft);
        };
        Ok(Self {
            context,
            action: resolved.intent.clone(),
            calldata: data.clone(),
            native_value: *value,
            amount: action_amount(&resolved.intent),
            estimated_fee,
            fingerprint: resolved.fingerprint,
        })
    }

    pub fn rebuild_matches(&self, rebuilt: &GovernanceResolvedAction) -> bool {
        rebuilt.fingerprint == self.fingerprint
            && self.context.fingerprint(&rebuilt.intent, &rebuilt.raw) == self.fingerprint
            && rebuilt.raw
                == (PublicTransactionIntent::Raw {
                    to: Some(self.context.contract),
                    value: self.native_value,
                    data: self.calldata.clone(),
                })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernancePreflight {
    pub action: GovernanceResolvedAction,
    pub estimate: PublicAdvancedTransactionEstimate,
}

pub fn governance_estimate_request(
    review: &GovernanceActionReview,
    effective_chain: Option<EffectiveChainConfig>,
    gas_fee: PublicActionGasFeeSelection,
) -> PublicAdvancedTransactionEstimateRequest {
    PublicAdvancedTransactionEstimateRequest {
        chain_id: review.context.chain_id,
        effective_chain,
        from: review.context.actor,
        intent: PublicTransactionIntent::Raw {
            to: Some(review.context.contract),
            value: review.native_value,
            data: review.calldata.clone(),
        },
        gas_fee,
        access_list: None,
    }
}

pub async fn simulate_governance_action(
    review: &GovernanceActionReview,
    rebuilt: GovernanceResolvedAction,
    effective_chain: Option<EffectiveChainConfig>,
    quote: crate::PublicActionGasFeeQuote,
    resolved_fee: crate::PublicActionResolvedGasFee,
    http: &HttpContext,
) -> std::result::Result<GovernancePreflight, GovernanceActionError> {
    if !review.rebuild_matches(&rebuilt) {
        return Err(GovernanceActionError::StaleDraft);
    }
    let estimate = simulate_public_advanced_transaction_with_fee(
        governance_estimate_request(
            review,
            effective_chain,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: resolved_fee.max_fee_per_gas,
                max_priority_fee_per_gas: resolved_fee.max_priority_fee_per_gas,
            },
        ),
        quote,
        resolved_fee,
        http,
    )
    .await
    .map_err(|error| match error {
        PublicAdvancedTransactionSimulationError::Reverted(reason)
        | PublicAdvancedTransactionSimulationError::Unavailable(reason) => {
            GovernanceActionError::Simulation(reason)
        }
    })?;
    Ok(GovernancePreflight {
        action: rebuilt,
        estimate,
    })
}

pub async fn estimate_governance_action(
    review: &GovernanceActionReview,
    rebuilt: GovernanceResolvedAction,
    effective_chain: Option<EffectiveChainConfig>,
    quote: crate::PublicActionGasFeeQuote,
    resolved_fee: crate::PublicActionResolvedGasFee,
    http: &HttpContext,
) -> Result<GovernancePreflight> {
    if !review.rebuild_matches(&rebuilt) {
        return Err(eyre!(GovernanceActionError::StaleDraft));
    }
    let estimate = estimate_public_advanced_transaction_with_fee(
        governance_estimate_request(
            review,
            effective_chain,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: resolved_fee.max_fee_per_gas,
                max_priority_fee_per_gas: resolved_fee.max_priority_fee_per_gas,
            },
        ),
        quote,
        resolved_fee,
        http,
    )
    .await?;
    Ok(GovernancePreflight {
        action: rebuilt,
        estimate,
    })
}

pub struct GovernanceSubmissionRequest {
    pub review: GovernanceActionReview,
    pub rebuilt: GovernanceResolvedAction,
    pub estimate: PublicAdvancedTransactionEstimate,
    pub public_send: PublicSendRequest,
    pub progress_step: PublicActionProgressStep,
}

/// The only multi-transaction Governance workflows. Direct Stake with sufficient allowance and
/// direct Unlock are ordinary exact-call submissions and must not be wrapped here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GovernanceWorkflow {
    StakeApproval(StakePlan),
    UndelegateThenUnlock(UndelegateFirstPlan),
}

pub struct GovernanceWorkflowRequest {
    pub initial: GovernanceSubmissionRequest,
    pub workflow: GovernanceWorkflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceWorkflowResult {
    pub transactions: Vec<PublicSendResult>,
}

/// Compare the public engine's authorization to the exact estimate used for this Governance
/// draft. The public submission engine performs its own fee-aware signer check afterwards.
pub fn validate_governance_authorization(
    authorization: Option<&PublicAdvancedTransactionAuthorization>,
    estimate: Option<&PublicAdvancedTransactionEstimate>,
) -> std::result::Result<(), GovernanceActionError> {
    let authorization = authorization.ok_or(GovernanceActionError::MissingAuthorization)?;
    let estimate = estimate.ok_or(GovernanceActionError::MissingEstimate)?;
    if authorization.payload_fingerprint != estimate.payload_fingerprint
        || authorization.gas_limit != estimate.gas_limit
    {
        return Err(GovernanceActionError::MismatchedAuthorization);
    }
    Ok(())
}

/// Pure admission gate used by the UI before constructing a submission request.  Keeping this
/// separate makes it possible to prove that missing authorization never reaches the signer.
pub fn validate_governance_submission(
    review: &GovernanceActionReview,
    rebuilt: &GovernanceResolvedAction,
    chain_id: u64,
    public_account_uuid: &str,
    intent: &PublicTransactionIntent,
    has_authorization: bool,
) -> std::result::Result<(), GovernanceActionError> {
    if !review.rebuild_matches(rebuilt) {
        return Err(GovernanceActionError::StaleDraft);
    }
    if chain_id != review.context.chain_id
        || public_account_uuid != review.context.public_account_uuid
        || intent != &rebuilt.raw
    {
        return Err(GovernanceActionError::WrongPublicContext);
    }
    if !has_authorization {
        return Err(GovernanceActionError::MissingAuthorization);
    }
    Ok(())
}

pub async fn submit_governance_action_with_progress(
    request: GovernanceSubmissionRequest,
    http: &HttpContext,
    mut progress: impl FnMut(crate::PublicActionProgressUpdate) + Send,
) -> Result<PublicSendResult> {
    validate_governance_submission(
        &request.review,
        &request.rebuilt,
        request.public_send.chain_id,
        &request.public_send.public_account_uuid,
        &request.public_send.intent,
        request.public_send.advanced_authorization.is_some(),
    )
    .map_err(|error| eyre!(error))?;
    validate_governance_authorization(
        request.public_send.advanced_authorization.as_ref(),
        Some(&request.estimate),
    )
    .map_err(|error| eyre!(error))?;
    let signer = vaulted_public_signer(
        &request.public_send.vault_store,
        &request.public_send.view_session,
        Some(request.public_send.vault_password.as_str()),
        &request.public_send.public_account_uuid,
        request
            .public_send
            .protected_software_seed_session
            .as_deref(),
        request.public_send.trezor_app_passphrase,
        request.public_send.trezor_pin_matrix_provider,
    )?;
    if signer.address() != request.review.context.actor {
        return Err(eyre!(GovernanceActionError::WrongPublicContext));
    }
    let mut command_rx = request.public_send.command_rx;
    let tx = submit_public_action_step_with_signer(
        request.progress_step,
        "public-action",
        "public action transaction",
        request.public_send.chain_id,
        request.public_send.effective_chain.as_ref(),
        &request.public_send.intent,
        &signer,
        request.public_send.advanced_authorization,
        false,
        request.public_send.gas_fee,
        &mut command_rx,
        request.public_send.event_tx.as_ref(),
        http,
        &mut progress,
    )
    .await?;
    Ok(PublicSendResult { tx })
}

/// Validate workflow identity before deriving a signer or allowing any signature attempt.
pub fn validate_governance_workflow(
    initial: &GovernanceSubmissionRequest,
    workflow: &GovernanceWorkflow,
) -> std::result::Result<(), GovernanceActionError> {
    validate_governance_submission(
        &initial.review,
        &initial.rebuilt,
        initial.public_send.chain_id,
        &initial.public_send.public_account_uuid,
        &initial.public_send.intent,
        initial.public_send.advanced_authorization.is_some(),
    )?;
    validate_governance_authorization(
        initial.public_send.advanced_authorization.as_ref(),
        Some(&initial.estimate),
    )?;
    let contracts = governance_contracts(initial.public_send.chain_id)
        .ok_or(GovernanceActionError::WrongPublicContext)?;
    let context = &initial.review.context;
    if context.chain_id != initial.public_send.chain_id
        || context.contract_kind
            != match workflow {
                GovernanceWorkflow::StakeApproval(_) => GovernanceContractKind::GovernanceToken,
                GovernanceWorkflow::UndelegateThenUnlock(_) => GovernanceContractKind::Staking,
            }
    {
        return Err(GovernanceActionError::WrongPublicContext);
    }
    match workflow {
        GovernanceWorkflow::StakeApproval(plan) => {
            let GovernanceActionIntent::GovernanceTokenApproval { spender, amount } =
                &initial.rebuilt.intent
            else {
                return Err(GovernanceActionError::InvalidWorkflow);
            };
            if !plan.requires_approval_confirmation
                || plan.actor != context.actor
                || plan.staking != contracts.staking
                || plan.approval.as_ref() != Some(&initial.rebuilt.intent)
                || plan.stake.is_some()
                || context.contract != contracts.governance_token
                || *spender != contracts.staking
                || *amount != plan.amount
                || context.observed_state != plan.observed_state
                || initial.progress_step != PublicActionProgressStep::GovernanceApprove
            {
                return Err(GovernanceActionError::InvalidWorkflow);
            }
        }
        GovernanceWorkflow::UndelegateThenUnlock(plan) => {
            let GovernanceActionIntent::Undelegate { stake_id } = &initial.rebuilt.intent else {
                return Err(GovernanceActionError::InvalidWorkflow);
            };
            if plan.owner != context.actor
                || plan.stake_id != *stake_id
                || plan.previous_delegate == plan.owner
                || plan.intent != initial.rebuilt.intent
                || context.contract != contracts.staking
                || context.observed_state != plan.observed_state
                || initial.progress_step != PublicActionProgressStep::Undelegate
            {
                return Err(GovernanceActionError::InvalidWorkflow);
            }
        }
    }
    Ok(())
}

/// Submit an approval-required Stake or externally delegated Unlock as one signer session.
/// Transaction two is deliberately resolved only after its first transaction confirms and fresh
/// chain reads succeed; it never reuses transaction one's exact-call authorization.
pub async fn submit_governance_workflow_with_progress(
    request: GovernanceWorkflowRequest,
    http: &HttpContext,
    mut progress: impl FnMut(crate::PublicActionProgressUpdate) + Send,
) -> Result<GovernanceWorkflowResult> {
    validate_governance_workflow(&request.initial, &request.workflow)
        .map_err(|error| eyre!(error))?;
    let GovernanceWorkflowRequest { initial, workflow } = request;
    let signer: VaultedPublicSigner = vaulted_public_signer(
        &initial.public_send.vault_store,
        &initial.public_send.view_session,
        Some(initial.public_send.vault_password.as_str()),
        &initial.public_send.public_account_uuid,
        initial
            .public_send
            .protected_software_seed_session
            .as_deref(),
        initial.public_send.trezor_app_passphrase,
        initial.public_send.trezor_pin_matrix_provider,
    )?;
    if signer.address() != initial.review.context.actor {
        return Err(eyre!(GovernanceActionError::WrongPublicContext));
    }
    let chain_id = initial.public_send.chain_id;
    let effective_chain = initial.public_send.effective_chain.as_ref();
    let actor = signer.address();
    let mut command_rx = initial.public_send.command_rx;
    let event_tx = initial.public_send.event_tx.as_ref();
    let first = submit_public_action_step_with_signer(
        initial.progress_step,
        "public-action",
        "public action transaction",
        chain_id,
        effective_chain,
        &initial.public_send.intent,
        &signer,
        initial.public_send.advanced_authorization,
        false,
        initial.public_send.gas_fee,
        &mut command_rx,
        event_tx,
        http,
        &mut progress,
    )
    .await?;
    let mut transactions = vec![PublicSendResult { tx: first }];

    let (next_intent, next_context, next_step) = match &workflow {
        GovernanceWorkflow::StakeApproval(plan) => {
            let Some((balance, allowance)) =
                fetch_governance_token_balance_allowance(chain_id, actor, effective_chain, http)
                    .await?
            else {
                return Err(eyre!(GovernanceActionError::FreshStateUnavailable));
            };
            let continued = continue_stake_after_approval(plan, balance, allowance)
                .map_err(|error| eyre!(error))?;
            let intent = continued
                .stake
                .ok_or_else(|| eyre!(GovernanceActionError::FreshStateUnavailable))?;
            let context = GovernanceActionContext {
                private_wallet_uuid: initial.review.context.private_wallet_uuid.clone(),
                chain_id,
                public_account_uuid: initial.review.context.public_account_uuid.clone(),
                actor,
                contract: continued.staking,
                contract_kind: GovernanceContractKind::Staking,
                observed_state: continued.observed_state,
            };
            (intent, context, PublicActionProgressStep::Stake)
        }
        GovernanceWorkflow::UndelegateThenUnlock(plan) => {
            let metrics = fetch_staking_global_metrics(chain_id, effective_chain, http)
                .await?
                .ok_or_else(|| eyre!(GovernanceActionError::FreshStateUnavailable))?;
            let account = fetch_account_stakes(
                chain_id,
                &[actor],
                metrics.chain_time,
                effective_chain,
                http,
                MulticallChunkSize::new(DEFAULT_MULTICALL_CHUNK_SIZE),
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre!(GovernanceActionError::FreshStateUnavailable))?;
            let stakes = account.stakes.map_err(|error| {
                eyre!("{}: {error}", GovernanceActionError::FreshStateUnavailable)
            })?;
            let fresh = stakes
                .into_iter()
                .find(|position| position.id == plan.stake_id)
                .ok_or_else(|| eyre!(GovernanceActionError::FreshStateUnavailable))?;
            let continued =
                continue_unlock_after_undelegation(plan, &fresh).map_err(|error| eyre!(error))?;
            let context = GovernanceActionContext {
                private_wallet_uuid: initial.review.context.private_wallet_uuid.clone(),
                chain_id,
                public_account_uuid: initial.review.context.public_account_uuid.clone(),
                actor,
                contract: initial.review.context.contract,
                contract_kind: GovernanceContractKind::Staking,
                observed_state: continued.observed_state,
            };
            (continued.intent, context, PublicActionProgressStep::Unlock)
        }
    };
    let resolved = next_intent
        .resolve(&next_context)
        .map_err(|error| eyre!(error))?;
    let second = submit_public_action_step_with_signer(
        next_step,
        "public-action",
        "public action transaction",
        chain_id,
        effective_chain,
        &resolved.raw,
        &signer,
        None,
        true,
        initial.public_send.gas_fee,
        &mut command_rx,
        event_tx,
        http,
        &mut progress,
    )
    .await?;
    transactions.push(PublicSendResult { tx: second });
    Ok(GovernanceWorkflowResult { transactions })
}

fn positive(amount: U256) -> std::result::Result<(), GovernanceActionError> {
    if amount.is_zero() {
        Err(GovernanceActionError::NonPositiveAmount)
    } else {
        Ok(())
    }
}

const fn action_amount(action: &GovernanceActionIntent) -> Option<U256> {
    match action {
        GovernanceActionIntent::Sponsor { amount, .. }
        | GovernanceActionIntent::Unsponsor { amount, .. }
        | GovernanceActionIntent::Yay { amount, .. }
        | GovernanceActionIntent::Nay { amount, .. }
        | GovernanceActionIntent::GovernanceTokenApproval { amount, .. }
        | GovernanceActionIntent::Stake { amount } => Some(*amount),
        GovernanceActionIntent::Delegate { .. }
        | GovernanceActionIntent::Undelegate { .. }
        | GovernanceActionIntent::Unlock { .. }
        | GovernanceActionIntent::PrincipalClaim { .. }
        | GovernanceActionIntent::CallVote { .. }
        | GovernanceActionIntent::RewardClaim { .. } => None,
    }
}

fn append_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn append_u256(output: &mut Vec<u8>, value: U256) {
    output.extend_from_slice(&value.to_be_bytes::<32>());
}

fn append_raw(output: &mut Vec<u8>, raw: &PublicTransactionIntent) {
    let PublicTransactionIntent::Raw { to, value, data } = raw else {
        return;
    };
    output.push(u8::from(to.is_some()));
    if let Some(to) = to {
        output.extend_from_slice(to.as_slice());
    }
    append_u256(output, *value);
    append_bytes(output, data);
}

fn append_intent(output: &mut Vec<u8>, intent: &GovernanceActionIntent) {
    // The discriminant and all parameter bytes are part of the domain-separated hash.  This is
    // intentionally explicit instead of relying on `Debug` or a serialisation format.
    output.push(match intent {
        GovernanceActionIntent::Sponsor { .. } => 0,
        GovernanceActionIntent::Unsponsor { .. } => 1,
        GovernanceActionIntent::CallVote { .. } => 2,
        GovernanceActionIntent::Yay { .. } => 3,
        GovernanceActionIntent::Nay { .. } => 4,
        GovernanceActionIntent::GovernanceTokenApproval { .. } => 5,
        GovernanceActionIntent::Stake { .. } => 6,
        GovernanceActionIntent::Delegate { .. } => 7,
        GovernanceActionIntent::Undelegate { .. } => 8,
        GovernanceActionIntent::Unlock { .. } => 9,
        GovernanceActionIntent::PrincipalClaim { .. } => 10,
        GovernanceActionIntent::RewardClaim { .. } => 11,
    });
    match intent {
        GovernanceActionIntent::Sponsor {
            proposal_version,
            proposal_index,
            amount,
            snapshot_interval,
            snapshot_hint,
        }
        | GovernanceActionIntent::Yay {
            proposal_version,
            proposal_index,
            amount,
            snapshot_interval,
            snapshot_hint,
        }
        | GovernanceActionIntent::Nay {
            proposal_version,
            proposal_index,
            amount,
            snapshot_interval,
            snapshot_hint,
        } => {
            output.push(version_byte(*proposal_version));
            append_u256(output, *proposal_index);
            append_u256(output, *amount);
            append_u256(output, *snapshot_interval);
            append_u256(output, *snapshot_hint);
        }
        GovernanceActionIntent::Unsponsor {
            proposal_version,
            proposal_index,
            amount,
        } => {
            output.push(version_byte(*proposal_version));
            append_u256(output, *proposal_index);
            append_u256(output, *amount);
        }
        GovernanceActionIntent::CallVote {
            proposal_version,
            proposal_index,
        } => {
            output.push(version_byte(*proposal_version));
            append_u256(output, *proposal_index);
        }
        GovernanceActionIntent::GovernanceTokenApproval { spender, amount } => {
            output.extend_from_slice(spender.as_slice());
            append_u256(output, *amount);
        }
        GovernanceActionIntent::Stake { amount } => append_u256(output, *amount),
        GovernanceActionIntent::Delegate { stake_id, delegate } => {
            append_u256(output, *stake_id);
            output.extend_from_slice(delegate.as_slice());
        }
        GovernanceActionIntent::Undelegate { stake_id }
        | GovernanceActionIntent::Unlock { stake_id }
        | GovernanceActionIntent::PrincipalClaim { stake_id } => append_u256(output, *stake_id),
        GovernanceActionIntent::RewardClaim {
            reward_tokens,
            starting_interval,
            ending_interval,
            snapshot_hints,
            expected_amounts,
        } => {
            append_u256(output, U256::from(reward_tokens.len()));
            for token in reward_tokens {
                output.extend_from_slice(token.as_slice());
            }
            append_u256(output, *starting_interval);
            append_u256(output, *ending_interval);
            append_u256(output, U256::from(snapshot_hints.len()));
            for hint in snapshot_hints {
                append_u256(output, *hint);
            }
            append_u256(output, U256::from(expected_amounts.len()));
            for amount in expected_amounts {
                append_u256(output, *amount);
            }
        }
    }
}

const fn version_byte(version: GovernanceContractVersion) -> u8 {
    match version {
        GovernanceContractVersion::V1 => 1,
        GovernanceContractVersion::V2 => 2,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceProgressStatus {
    Pending,
    Submitted,
    Confirmed,
    Failed,
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProgressStep {
    pub fingerprint: B256,
    pub transaction_hash: Option<B256>,
    pub status: GovernanceProgressStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProgress {
    pub steps: Vec<GovernanceProgressStep>,
    pub status: GovernanceProgressStatus,
    pub dismissed: bool,
    handed_off: bool,
}

impl GovernanceProgress {
    #[must_use]
    pub fn new(fingerprints: impl IntoIterator<Item = B256>) -> Self {
        let steps = fingerprints
            .into_iter()
            .map(|fingerprint| GovernanceProgressStep {
                fingerprint,
                transaction_hash: None,
                status: GovernanceProgressStatus::Pending,
            })
            .collect();
        Self {
            steps,
            status: GovernanceProgressStatus::Pending,
            dismissed: false,
            handed_off: false,
        }
    }
    pub fn record_submission(&mut self, index: usize, transaction_hash: B256) -> bool {
        let Some(step) = self.steps.get_mut(index) else {
            return false;
        };
        step.transaction_hash = Some(transaction_hash);
        step.status = GovernanceProgressStatus::Submitted;
        self.status = GovernanceProgressStatus::Submitted;
        self.handed_off = true;
        true
    }
    pub fn add_step(&mut self, fingerprint: B256) -> bool {
        if matches!(
            self.status,
            GovernanceProgressStatus::Failed | GovernanceProgressStatus::Canceled
        ) {
            return false;
        }
        self.steps.push(GovernanceProgressStep {
            fingerprint,
            transaction_hash: None,
            status: GovernanceProgressStatus::Pending,
        });
        self.recompute(true);
        true
    }
    pub fn record_confirmation(
        &mut self,
        index: usize,
        transaction_hash: B256,
        continuation_expected: bool,
    ) -> bool {
        let Some(step) = self.steps.get_mut(index) else {
            return false;
        };
        step.transaction_hash = Some(transaction_hash);
        step.status = GovernanceProgressStatus::Confirmed;
        self.handed_off = true;
        self.recompute(continuation_expected);
        true
    }
    pub fn record_failure(&mut self, index: usize, transaction_hash: Option<B256>) -> bool {
        let Some(step) = self.steps.get_mut(index) else {
            return false;
        };
        if transaction_hash.is_some() {
            step.transaction_hash = transaction_hash;
        }
        step.status = GovernanceProgressStatus::Failed;
        self.status = GovernanceProgressStatus::Failed;
        self.handed_off = true;
        true
    }
    pub fn finish_continuation(&mut self) {
        self.recompute(false);
    }
    fn recompute(&mut self, continuation_expected: bool) {
        if self
            .steps
            .iter()
            .any(|step| step.status == GovernanceProgressStatus::Failed)
        {
            self.status = GovernanceProgressStatus::Failed;
        } else if continuation_expected
            || self.steps.iter().any(|step| {
                matches!(
                    step.status,
                    GovernanceProgressStatus::Pending | GovernanceProgressStatus::Submitted
                )
            })
        {
            self.status = GovernanceProgressStatus::Pending;
        } else if !self.steps.is_empty()
            && self
                .steps
                .iter()
                .all(|step| step.status == GovernanceProgressStatus::Confirmed)
        {
            self.status = GovernanceProgressStatus::Confirmed;
        } else {
            self.status = GovernanceProgressStatus::Pending;
        }
    }
    pub const fn close(&mut self) {
        self.dismissed = true;
    }
    /// Cancel only a draft which has not been handed to a signer or broadcaster.
    pub const fn cancel(&mut self) -> bool {
        if self.handed_off {
            return false;
        }
        self.status = GovernanceProgressStatus::Canceled;
        true
    }
    #[must_use]
    pub const fn handed_off(&self) -> bool {
        self.handed_off
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn context() -> GovernanceActionContext {
        GovernanceActionContext {
            private_wallet_uuid: "wallet".into(),
            chain_id: 1,
            public_account_uuid: "account".into(),
            actor: address!("1111111111111111111111111111111111111111"),
            contract: address!("2222222222222222222222222222222222222222"),
            contract_kind: GovernanceContractKind::Voting,
            observed_state: B256::ZERO,
        }
    }
    fn sponsor() -> GovernanceActionIntent {
        GovernanceActionIntent::Sponsor {
            proposal_version: GovernanceContractVersion::V2,
            proposal_index: U256::from(7),
            amount: U256::from(3),
            snapshot_interval: U256::from(9),
            snapshot_hint: U256::from(2),
        }
    }

    #[test]
    fn each_context_identity_changes_fingerprint() {
        let base = context();
        let raw = sponsor().resolve(&base).unwrap().raw;
        let fp = base.fingerprint(&sponsor(), &raw);
        let mut changed = base.clone();
        changed.private_wallet_uuid = "other".into();
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
        changed = base.clone();
        changed.chain_id = 56;
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
        changed = base.clone();
        changed.public_account_uuid = "other".into();
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
        changed = base.clone();
        changed.actor = address!("3333333333333333333333333333333333333333");
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
        changed = base.clone();
        changed.contract = address!("3333333333333333333333333333333333333333");
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
        changed = base;
        changed.observed_state = B256::from([1_u8; 32]);
        assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    }

    #[test]
    fn variants_and_raw_calls_are_distinct_and_zero_value() {
        let ctx = context();
        let sponsor = sponsor().resolve(&ctx).unwrap();
        let nay = GovernanceActionIntent::Nay {
            proposal_version: GovernanceContractVersion::V2,
            proposal_index: U256::from(7),
            amount: U256::from(3),
            snapshot_interval: U256::from(9),
            snapshot_hint: U256::from(2),
        }
        .resolve(&ctx)
        .unwrap();
        assert_ne!(sponsor.fingerprint, nay.fingerprint);
        assert!(
            matches!(sponsor.raw, PublicTransactionIntent::Raw { to: Some(_), value, .. } if value.is_zero())
        );
        let PublicTransactionIntent::Raw { data, .. } = sponsor.raw else {
            unreachable!()
        };
        let decoded = GovernanceVotingActions::sponsorProposalCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.account, ctx.actor);
        assert_eq!(decoded.proposalID, U256::from(7));
        assert_eq!(decoded.hint, U256::from(2));
    }

    #[test]
    fn reward_call_binds_ordered_tokens_amounts_actor_range_and_hints() {
        let mut ctx = context();
        ctx.contract_kind = GovernanceContractKind::GovernorRewards;
        let action = GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![
                address!("1111111111111111111111111111111111111111"),
                address!("3333333333333333333333333333333333333333"),
            ],
            starting_interval: U256::from(4),
            ending_interval: U256::from(5),
            snapshot_hints: vec![U256::from(8), U256::from(9)],
            expected_amounts: vec![U256::from(10), U256::ZERO],
        };
        let resolved = action.resolve(&ctx).unwrap();
        let PublicTransactionIntent::Raw { data, .. } = resolved.raw else {
            unreachable!()
        };
        let decoded = GovernanceRewardsActions::claimCall::abi_decode(&data).unwrap();
        assert_eq!(
            decoded.tokens,
            vec![
                address!("1111111111111111111111111111111111111111"),
                address!("3333333333333333333333333333333333333333")
            ]
        );
        assert_eq!(decoded.account, ctx.actor);
        assert_eq!(decoded.startingInterval, U256::from(4));
        assert_eq!(decoded.endingInterval, U256::from(5));
        assert_eq!(decoded.hints, vec![U256::from(8), U256::from(9)]);
        let mut changed = action;
        if let GovernanceActionIntent::RewardClaim {
            expected_amounts, ..
        } = &mut changed
        {
            expected_amounts[0] = U256::from(11);
        }
        assert_ne!(
            resolved.fingerprint,
            changed.resolve(&ctx).unwrap().fingerprint
        );
    }

    #[test]
    fn reward_call_rejects_invalid_token_vectors() {
        let mut ctx = context();
        ctx.contract_kind = GovernanceContractKind::GovernorRewards;
        let first_token = address!("1111111111111111111111111111111111111111");
        for action in [
            GovernanceActionIntent::RewardClaim {
                reward_tokens: Vec::new(),
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                snapshot_hints: vec![U256::ZERO],
                expected_amounts: Vec::new(),
            },
            GovernanceActionIntent::RewardClaim {
                reward_tokens: vec![first_token],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                snapshot_hints: vec![U256::ZERO],
                expected_amounts: vec![],
            },
        ] {
            assert!(matches!(
                action.resolve(&ctx),
                Err(GovernanceActionError::InvalidRewardAmounts)
            ));
        }
        assert!(matches!(
            (GovernanceActionIntent::RewardClaim {
                reward_tokens: vec![
                    address!("3333333333333333333333333333333333333333"),
                    address!("1111111111111111111111111111111111111111")
                ],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                snapshot_hints: vec![U256::ZERO],
                expected_amounts: vec![U256::ONE, U256::ONE],
            })
            .resolve(&ctx),
            Err(GovernanceActionError::InvalidRewardTokenOrder)
        ));
        assert!(matches!(
            (GovernanceActionIntent::RewardClaim {
                reward_tokens: vec![first_token; 2],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                snapshot_hints: vec![U256::ZERO],
                expected_amounts: vec![U256::ONE, U256::ONE],
            })
            .resolve(&ctx),
            Err(GovernanceActionError::InvalidRewardTokenOrder)
        ));
        assert!(matches!(
            (GovernanceActionIntent::RewardClaim {
                reward_tokens: vec![first_token],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                snapshot_hints: vec![U256::ZERO],
                expected_amounts: vec![U256::ZERO],
            })
            .resolve(&ctx),
            Err(GovernanceActionError::ZeroReward)
        ));
    }

    #[test]
    fn stale_rebuild_and_missing_authorization_are_rejected_before_submission() {
        let ctx = context();
        let resolved = sponsor().resolve(&ctx).unwrap();
        let review = GovernanceActionReview::from_resolved(&resolved, ctx.clone(), None).unwrap();
        let mut inconsistent = resolved.clone();
        inconsistent.fingerprint = B256::from([6_u8; 32]);
        assert!(!review.rebuild_matches(&inconsistent));
        let mut changed = ctx.clone();
        changed.observed_state = B256::from([4_u8; 32]);
        let rebuilt = sponsor().resolve(&changed).unwrap();
        assert_eq!(
            validate_governance_submission(&review, &rebuilt, 1, "account", &rebuilt.raw, true),
            Err(GovernanceActionError::StaleDraft)
        );
        assert_eq!(
            validate_governance_submission(&review, &resolved, 1, "account", &resolved.raw, false),
            Err(GovernanceActionError::MissingAuthorization)
        );
        assert_eq!(
            validate_governance_submission(&review, &resolved, 56, "account", &resolved.raw, true),
            Err(GovernanceActionError::WrongPublicContext)
        );
        assert_eq!(
            validate_governance_submission(&review, &resolved, 1, "other", &resolved.raw, true),
            Err(GovernanceActionError::WrongPublicContext)
        );
        let other_raw = PublicTransactionIntent::Raw {
            to: Some(ctx.contract),
            value: U256::ZERO,
            data: Bytes::from_static(&[1]),
        };
        assert_eq!(
            validate_governance_submission(&review, &resolved, 1, "account", &other_raw, true),
            Err(GovernanceActionError::WrongPublicContext)
        );
    }

    #[test]
    fn authorization_must_match_the_current_public_estimate() {
        let estimate = PublicAdvancedTransactionEstimate {
            payload_fingerprint: B256::from([1_u8; 32]),
            raw_gas_limit: 1,
            gas_limit: 2,
            max_fee_per_gas: 3,
            max_priority_fee_per_gas: 4,
            expected_fee_per_gas: 3,
            expected_gas_cost: U256::from(3),
            max_gas_cost: U256::from(6),
        };
        let matching = PublicAdvancedTransactionAuthorization {
            payload_fingerprint: estimate.payload_fingerprint,
            gas_limit: estimate.gas_limit,
        };
        assert!(validate_governance_authorization(Some(&matching), Some(&estimate)).is_ok());
        let mismatched = PublicAdvancedTransactionAuthorization {
            payload_fingerprint: B256::from([9_u8; 32]),
            gas_limit: estimate.gas_limit,
        };
        assert_eq!(
            validate_governance_authorization(Some(&mismatched), Some(&estimate)),
            Err(GovernanceActionError::MismatchedAuthorization)
        );
        assert_eq!(
            validate_governance_authorization(None, Some(&estimate)),
            Err(GovernanceActionError::MissingAuthorization)
        );
    }

    #[test]
    fn action_cannot_target_a_different_contract_class() {
        let mut ctx = context();
        ctx.contract_kind = GovernanceContractKind::Staking;
        assert_eq!(
            sponsor().resolve(&ctx),
            Err(GovernanceActionError::WrongContractKind)
        );
    }

    #[test]
    fn close_after_submission_preserves_submitted_state() {
        let mut progress = GovernanceProgress::new([B256::from([7_u8; 32])]);
        assert!(progress.record_submission(0, B256::from([8_u8; 32])));
        progress.close();
        assert!(progress.dismissed);
        assert!(progress.handed_off());
        assert_eq!(progress.status, GovernanceProgressStatus::Submitted);
    }

    #[test]
    fn confirmed_progress_tracks_continuations_and_failures() {
        let first = B256::from([7_u8; 32]);
        let first_hash = B256::from([8_u8; 32]);
        let second = B256::from([9_u8; 32]);
        let second_hash = B256::from([10_u8; 32]);
        let mut progress = GovernanceProgress::new([first]);
        assert!(progress.record_confirmation(0, first_hash, true));
        assert_eq!(progress.status, GovernanceProgressStatus::Pending);
        assert!(progress.add_step(second));
        assert!(progress.record_confirmation(1, second_hash, false));
        assert_eq!(progress.status, GovernanceProgressStatus::Confirmed);
        assert_eq!(progress.steps[0].transaction_hash, Some(first_hash));

        let mut failed = GovernanceProgress::new([first]);
        assert!(failed.record_confirmation(0, first_hash, true));
        assert!(failed.add_step(second));
        assert!(failed.record_failure(1, None));
        assert_eq!(failed.status, GovernanceProgressStatus::Failed);
        assert_eq!(failed.steps[0].transaction_hash, Some(first_hash));
        assert_eq!(failed.steps[1].transaction_hash, None);
    }
}
