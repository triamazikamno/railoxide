//! Read-only enumeration of the RAILGUN governance voting contracts.

use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::BlockHeader as _;
use alloy::eips::BlockNumberOrTag;
use alloy::network::BlockResponse as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::bindings::IMulticall3;
use alloy::providers::{CallItem, Provider};
use alloy::sol;
use alloy::sol_types::SolCall;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, WrapErr, eyre};
use railgun_ui::governance_contracts;
use sync_service::ChainConfigDefaults;
use thiserror::Error;
use tokio::time::timeout;

use crate::settings::EffectiveChainConfig;
use crate::staking::{MulticallChunkSize, fetch_account_snapshots_multi, snapshot_hint};
use crate::{HttpContext, effective_rpc_urls_for_chain, query_rpc_pool_with_http_client};

const GOVERNANCE_RPC_TIMEOUT: Duration = Duration::from_secs(30);

sol! {
    interface GovernanceVoting {
        function proposalsLength() external view returns (uint256);
        function PROPOSAL_SPONSOR_THRESHOLD() external view returns (uint256);
        function QUORUM() external view returns (uint256);
        function SPONSOR_WINDOW() external view returns (uint256);
        function VOTING_START_OFFSET() external view returns (uint256);
        function VOTING_YAY_END_OFFSET() external view returns (uint256);
        function VOTING_NAY_END_OFFSET() external view returns (uint256);
        function EXECUTION_START_OFFSET() external view returns (uint256);
        function EXECUTION_END_OFFSET() external view returns (uint256);
        function getSponsored(uint256 proposalID, address account) external view returns (uint256);
        function getVotes(uint256 proposalID, address account) external view returns (uint256);
        function lastSponsored(address account) external view returns (uint256 lastSponsorTime, uint256 proposalID);
        function sponsorProposal(uint256 proposalID, uint256 amount, address account, uint256 hint) external;
        function unsponsorProposal(uint256 proposalID, uint256 amount, address account) external;
        function callVote(uint256 proposalID) external;
        function vote(uint256 proposalID, uint256 amount, bool yay, address account, uint256 hint) external;
        function proposals(uint256 index) external view returns (
            address proposer,
            string proposalDocument,
            uint256 publishTime,
            uint256 voteCallTime,
            uint256 sponsorship,
            bool executed,
            uint256 yayVotes,
            uint256 nayVotes,
            uint256 sponsorInterval,
            uint256 votingInterval
        );
        function getActions(uint256 id) external view returns (
            tuple(address, bytes, uint256)[]
        );
    }

    interface GovernanceVotingV2 {
        function proposals(uint256 index) external view returns (
            bool executed,
            address proposer,
            string proposalDocument,
            uint256 publishTime,
            uint256 voteCallTime,
            uint256 sponsorship,
            uint256 yayVotes,
            uint256 nayVotes,
            uint256 sponsorInterval,
            uint256 votingInterval
        );
    }

    interface GovernanceStaking {
        function votingPower(address owner) external view returns (uint256);
        function accountSnapshotAt(address owner, uint256 index, uint256 hint)
            external view returns (uint256 interval, uint256 votingPower);
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GovernanceContractVersion {
    V2,
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceContractSummary {
    pub version: GovernanceContractVersion,
    pub address: Address,
    pub proposal_count: U256,
    pub rules: GovernanceContractRules,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceContractRules {
    pub sponsor_threshold: U256,
    pub quorum: U256,
    pub sponsor_window: U256,
    pub voting_start_offset: U256,
    pub voting_yay_end_offset: U256,
    pub voting_nay_end_offset: U256,
    pub execution_start_offset: U256,
    pub execution_end_offset: U256,
}

impl GovernanceContractRules {
    pub fn validate(&self) -> Result<()> {
        if self.voting_start_offset >= self.voting_yay_end_offset
            || self.voting_yay_end_offset > self.voting_nay_end_offset
            || self.voting_nay_end_offset > self.execution_start_offset
            || self.execution_start_offset >= self.execution_end_offset
        {
            return Err(eyre!(
                "governance voting and execution offsets are inconsistent"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceOverview {
    pub chain_id: u64,
    pub v2: GovernanceContractSummary,
    pub v1: Option<GovernanceContractSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProposal {
    pub contract_version: GovernanceContractVersion,
    pub index: U256,
    pub contract_address: Address,
    pub proposer: Address,
    pub proposal_document: String,
    pub publish_time: U256,
    pub vote_call_time: U256,
    pub sponsorship: U256,
    pub executed: bool,
    pub yay_votes: U256,
    pub nay_votes: U256,
    pub sponsor_snapshot_interval: U256,
    pub voting_snapshot_interval: U256,
    pub actions: Vec<GovernanceProposalAction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProposalAction {
    pub call_contract: Address,
    pub calldata: Bytes,
    pub value: U256,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceProposalStage {
    Executed,
    AwaitingSponsorship,
    ReadyToCallVote,
    SponsorshipExpired,
    VoteCallExpired,
    VotingDelay,
    VotingOpen,
    NayOnlyVoting,
    Failed,
    PassedAwaitingExecution,
    PassedExecutable,
    ExecutionExpired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceQuorumBasis {
    AffirmativeOnly,
    TotalVotes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceMajorityResult {
    Yay,
    Nay,
    Tie,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProposalDeadlines {
    pub sponsorship: U256,
    pub voting_start: Option<U256>,
    pub yay_end: Option<U256>,
    pub nay_end: Option<U256>,
    pub execution_start: Option<U256>,
    pub execution_end: Option<U256>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceProposalStatus {
    pub stage: GovernanceProposalStage,
    pub deadlines: GovernanceProposalDeadlines,
    pub quorum_basis: GovernanceQuorumBasis,
    pub quorum: U256,
    pub quorum_progress: U256,
    pub quorum_met: bool,
    pub majority: GovernanceMajorityResult,
}

/// The deployed Voting contracts require a strictly greater-than seven-day gap between
/// sponsorships of different proposals.
pub const GOVERNANCE_SPONSOR_LOCKOUT_TIME_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceAccountSnapshot {
    pub interval: U256,
    pub voting_power: U256,
    pub hint: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceLastSponsored {
    pub last_sponsor_time: U256,
    pub proposal_id: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceParticipation {
    pub proposal_version: GovernanceContractVersion,
    pub proposal_id: U256,
    pub voting_contract: Address,
    pub account: Address,
    pub current_voting_power: U256,
    pub sponsorship_snapshot: GovernanceAccountSnapshot,
    pub voting_snapshot: GovernanceAccountSnapshot,
    pub sponsored: U256,
    pub voted: U256,
    pub last_sponsored: GovernanceLastSponsored,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceParticipationRow {
    pub account: Address,
    pub state: std::result::Result<GovernanceParticipation, GovernanceParticipationError>,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GovernanceParticipationError {
    #[error("unsupported governance chain {chain_id}")]
    UnsupportedChain { chain_id: u64 },
    #[error("governance {version:?} contract is unavailable on chain {chain_id}")]
    MissingContract {
        chain_id: u64,
        version: GovernanceContractVersion,
    },
    #[error("account snapshot is unavailable: {0}")]
    SnapshotUnavailable(String),
    #[error("account snapshot hint is invalid: {0}")]
    InvalidSnapshotHint(String),
    #[error("governance participation {field} read failed: {reason}")]
    Read { field: &'static str, reason: String },
    #[error("governance participation {field} ABI decode failed: {reason}")]
    Decode { field: &'static str, reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceCapacity {
    pub snapshot_power: Option<U256>,
    pub allocated: U256,
    pub remaining: Option<U256>,
    pub hint: Option<U256>,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GovernanceCapacityError {
    #[error("proposal snapshot is unavailable")]
    SnapshotUnavailable,
    #[error("allocated governance amount exceeds snapshot power")]
    AllocatedExceedsSnapshot,
}

impl GovernanceParticipation {
    pub fn sponsorship_capacity(
        &self,
    ) -> std::result::Result<GovernanceCapacity, GovernanceCapacityError> {
        calculate_governance_capacity(Some(&self.sponsorship_snapshot), self.sponsored)
    }

    pub fn voting_capacity(
        &self,
    ) -> std::result::Result<GovernanceCapacity, GovernanceCapacityError> {
        calculate_governance_capacity(Some(&self.voting_snapshot), self.voted)
    }
}

pub fn calculate_governance_capacity(
    snapshot: Option<&GovernanceAccountSnapshot>,
    allocated: U256,
) -> std::result::Result<GovernanceCapacity, GovernanceCapacityError> {
    let Some(snapshot) = snapshot else {
        return Err(GovernanceCapacityError::SnapshotUnavailable);
    };
    let remaining = snapshot
        .voting_power
        .checked_sub(allocated)
        .ok_or(GovernanceCapacityError::AllocatedExceedsSnapshot)?;
    Ok(GovernanceCapacity {
        snapshot_power: Some(snapshot.voting_power),
        allocated,
        remaining: Some(remaining),
        hint: Some(snapshot.hint),
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GovernanceCallError {
    #[error("unsupported governance chain {chain_id}")]
    UnsupportedChain { chain_id: u64 },
    #[error("legacy Voting contract is unavailable on chain {chain_id}")]
    MissingLegacyContract { chain_id: u64 },
    #[error("transaction account is the zero address")]
    ZeroAccount,
    #[error("transaction amount must be nonzero")]
    ZeroAmount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceTransactionCall {
    pub to: Address,
    pub data: Bytes,
    pub value: U256,
}

fn governance_voting_address(
    chain_id: u64,
    version: GovernanceContractVersion,
) -> std::result::Result<Address, GovernanceCallError> {
    let contracts =
        governance_contracts(chain_id).ok_or(GovernanceCallError::UnsupportedChain { chain_id })?;
    match version {
        GovernanceContractVersion::V2 => Ok(contracts.voting),
        GovernanceContractVersion::V1 => contracts
            .voting_legacy
            .ok_or(GovernanceCallError::MissingLegacyContract { chain_id }),
    }
}

fn validate_call_inputs(
    chain_id: u64,
    version: GovernanceContractVersion,
    account: Address,
    amount: Option<U256>,
) -> std::result::Result<Address, GovernanceCallError> {
    if account == Address::ZERO {
        return Err(GovernanceCallError::ZeroAccount);
    }
    if amount.is_some_and(|amount| amount.is_zero()) {
        return Err(GovernanceCallError::ZeroAmount);
    }
    governance_voting_address(chain_id, version)
}

pub fn build_sponsor_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    amount: U256,
    account: Address,
    hint: U256,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    let to = validate_call_inputs(chain_id, version, account, Some(amount))?;
    Ok(GovernanceTransactionCall {
        to,
        data: GovernanceVoting::sponsorProposalCall {
            proposalID: proposal_id,
            amount,
            account,
            hint,
        }
        .abi_encode()
        .into(),
        value: U256::ZERO,
    })
}

pub fn build_unsponsor_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    amount: U256,
    account: Address,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    let to = validate_call_inputs(chain_id, version, account, Some(amount))?;
    Ok(GovernanceTransactionCall {
        to,
        data: GovernanceVoting::unsponsorProposalCall {
            proposalID: proposal_id,
            amount,
            account,
        }
        .abi_encode()
        .into(),
        value: U256::ZERO,
    })
}

pub fn build_call_vote_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    account: Address,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    let to = validate_call_inputs(chain_id, version, account, None)?;
    Ok(GovernanceTransactionCall {
        to,
        data: GovernanceVoting::callVoteCall {
            proposalID: proposal_id,
        }
        .abi_encode()
        .into(),
        value: U256::ZERO,
    })
}

pub fn build_vote_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    amount: U256,
    yay: bool,
    account: Address,
    hint: U256,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    let to = validate_call_inputs(chain_id, version, account, Some(amount))?;
    Ok(GovernanceTransactionCall {
        to,
        data: GovernanceVoting::voteCall {
            proposalID: proposal_id,
            amount,
            yay,
            account,
            hint,
        }
        .abi_encode()
        .into(),
        value: U256::ZERO,
    })
}

pub fn build_yay_vote_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    amount: U256,
    account: Address,
    hint: U256,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    build_vote_call(chain_id, version, proposal_id, amount, true, account, hint)
}

pub fn build_nay_vote_call(
    chain_id: u64,
    version: GovernanceContractVersion,
    proposal_id: U256,
    amount: U256,
    account: Address,
    hint: U256,
) -> std::result::Result<GovernanceTransactionCall, GovernanceCallError> {
    build_vote_call(chain_id, version, proposal_id, amount, false, account, hint)
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GovernanceGuardError {
    #[error("chain timestamp is unavailable")]
    ChainTimeUnavailable,
    #[error("proposal is not in the required governance stage")]
    WrongStage,
    #[error("requested amount must be nonzero")]
    ZeroAmount,
    #[error("requested amount exceeds the recorded allocation")]
    AmountExceedsAllocation,
    #[error("sponsorship cooldown evidence is inconsistent")]
    InvalidCooldownEvidence,
    #[error("sponsorship cooldown has not elapsed")]
    SponsorshipCooldown,
}

fn require_chain_time(chain_time: Option<U256>) -> std::result::Result<U256, GovernanceGuardError> {
    chain_time.ok_or(GovernanceGuardError::ChainTimeUnavailable)
}

fn require_sponsorship_stage(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
) -> std::result::Result<U256, GovernanceGuardError> {
    let chain_time = require_chain_time(chain_time)?;
    let stage = derive_governance_proposal_stage(proposal, rules, chain_time)
        .map_err(|_| GovernanceGuardError::WrongStage)?;
    if matches!(
        stage,
        GovernanceProposalStage::AwaitingSponsorship | GovernanceProposalStage::ReadyToCallVote
    ) {
        Ok(chain_time)
    } else {
        Err(GovernanceGuardError::WrongStage)
    }
}

pub fn guard_sponsor(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
    amount: U256,
    last_sponsored: Option<&GovernanceLastSponsored>,
) -> std::result::Result<(), GovernanceGuardError> {
    let chain_time = require_sponsorship_stage(proposal, rules, chain_time)?;
    if amount.is_zero() {
        return Err(GovernanceGuardError::ZeroAmount);
    }
    if let Some(last) = last_sponsored {
        if chain_time < last.last_sponsor_time {
            return Err(GovernanceGuardError::InvalidCooldownEvidence);
        }
        if last.proposal_id != proposal.index
            && chain_time - last.last_sponsor_time
                <= U256::from(GOVERNANCE_SPONSOR_LOCKOUT_TIME_SECONDS)
        {
            return Err(GovernanceGuardError::SponsorshipCooldown);
        }
    }
    Ok(())
}

pub fn guard_unsponsor(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
    amount: U256,
    sponsored: U256,
) -> std::result::Result<(), GovernanceGuardError> {
    require_sponsorship_stage(proposal, rules, chain_time)?;
    if amount.is_zero() {
        return Err(GovernanceGuardError::ZeroAmount);
    }
    if amount > sponsored {
        return Err(GovernanceGuardError::AmountExceedsAllocation);
    }
    Ok(())
}

pub fn guard_call_vote(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
) -> std::result::Result<(), GovernanceGuardError> {
    let chain_time = require_chain_time(chain_time)?;
    let stage = derive_governance_proposal_stage(proposal, rules, chain_time)
        .map_err(|_| GovernanceGuardError::WrongStage)?;
    if stage == GovernanceProposalStage::ReadyToCallVote {
        Ok(())
    } else {
        Err(GovernanceGuardError::WrongStage)
    }
}

pub fn guard_yay_vote(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
    amount: U256,
) -> std::result::Result<(), GovernanceGuardError> {
    let chain_time = require_chain_time(chain_time)?;
    if amount.is_zero() {
        return Err(GovernanceGuardError::ZeroAmount);
    }
    let stage = derive_governance_proposal_stage(proposal, rules, chain_time)
        .map_err(|_| GovernanceGuardError::WrongStage)?;
    if stage == GovernanceProposalStage::VotingOpen {
        Ok(())
    } else {
        Err(GovernanceGuardError::WrongStage)
    }
}

pub fn guard_nay_vote(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: Option<U256>,
    amount: U256,
) -> std::result::Result<(), GovernanceGuardError> {
    let chain_time = require_chain_time(chain_time)?;
    if amount.is_zero() {
        return Err(GovernanceGuardError::ZeroAmount);
    }
    let stage = derive_governance_proposal_stage(proposal, rules, chain_time)
        .map_err(|_| GovernanceGuardError::WrongStage)?;
    if matches!(
        stage,
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting
    ) {
        Ok(())
    } else {
        Err(GovernanceGuardError::WrongStage)
    }
}

/// Fetch account-specific proposal state. Snapshot discovery reads the configured Staking
/// contract first, then one bounded multicall reads Staking's current and historical power plus
/// the proposal's version-specific Voting contract state. A failed account row does not discard
/// the proposal or other account rows.
pub async fn fetch_governance_participation(
    chain_id: u64,
    proposal: &GovernanceProposal,
    accounts: &[Address],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Vec<GovernanceParticipationRow>> {
    let contracts = governance_contracts(chain_id)
        .ok_or_else(|| eyre!("unsupported governance chain {chain_id}"))?;
    let voting_address = governance_voting_address(chain_id, proposal.contract_version)
        .map_err(|error| eyre!("{error}"))?;
    if proposal.contract_address != voting_address {
        return Err(eyre!(
            "proposal identity routes to {expected}, but proposal stores {actual}",
            expected = voting_address,
            actual = proposal.contract_address,
        ));
    }

    let mut rows = vec![None; accounts.len()];
    let mut prepared = Vec::new();
    let snapshot_results = fetch_account_snapshots_multi(
        chain_id,
        accounts,
        effective_chain,
        http,
        MulticallChunkSize::default(),
    )
    .await?;
    for (row_index, (&account, snapshot_result)) in
        accounts.iter().zip(snapshot_results).enumerate()
    {
        match snapshot_result.snapshots {
            Ok(snapshots) => {
                let sponsor_hint = snapshot_hint(&snapshots, proposal.sponsor_snapshot_interval);
                let voting_hint = snapshot_hint(&snapshots, proposal.voting_snapshot_interval);
                match (sponsor_hint, voting_hint) {
                    (Ok(sponsor_hint), Ok(voting_hint)) => {
                        prepared.push((row_index, account, sponsor_hint, voting_hint));
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        rows[row_index] = Some(GovernanceParticipationRow {
                            account,
                            state: Err(GovernanceParticipationError::InvalidSnapshotHint(
                                error.to_string(),
                            )),
                        });
                    }
                }
            }
            Err(error) => {
                rows[row_index] = Some(GovernanceParticipationRow {
                    account,
                    state: Err(GovernanceParticipationError::SnapshotUnavailable(error)),
                });
            }
        }
    }
    if prepared.is_empty() {
        return Ok(rows.into_iter().flatten().collect());
    }

    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let accounts_per_batch = (MulticallChunkSize::default().get() / 6).max(1);
    let mut values = Vec::new();
    for prepared_batch in prepared.chunks(accounts_per_batch) {
        let expected_batch = prepared_batch
            .len()
            .checked_mul(6)
            .ok_or_else(|| eyre!("governance participation result count overflows"))?;
        let calls = prepared_batch
            .iter()
            .flat_map(|&(_, account, sponsor_hint, voting_hint)| {
                [
                    (
                        contracts.staking,
                        GovernanceStaking::votingPowerCall { owner: account }
                            .abi_encode()
                            .into(),
                    ),
                    (
                        contracts.staking,
                        GovernanceStaking::accountSnapshotAtCall {
                            owner: account,
                            index: proposal.sponsor_snapshot_interval,
                            hint: sponsor_hint,
                        }
                        .abi_encode()
                        .into(),
                    ),
                    (
                        contracts.staking,
                        GovernanceStaking::accountSnapshotAtCall {
                            owner: account,
                            index: proposal.voting_snapshot_interval,
                            hint: voting_hint,
                        }
                        .abi_encode()
                        .into(),
                    ),
                    (
                        voting_address,
                        GovernanceVoting::getSponsoredCall {
                            proposalID: proposal.index,
                            account,
                        }
                        .abi_encode()
                        .into(),
                    ),
                    (
                        voting_address,
                        GovernanceVoting::getVotesCall {
                            proposalID: proposal.index,
                            account,
                        }
                        .abi_encode()
                        .into(),
                    ),
                    (
                        voting_address,
                        GovernanceVoting::lastSponsoredCall { account }
                            .abi_encode()
                            .into(),
                    ),
                ]
            })
            .collect();
        let batch_values = fetch_governance_multicall_raw(&pool, multicall_address, calls).await?;
        if batch_values.len() != expected_batch {
            return Err(eyre!(
                "governance participation multicall returned {}, expected {}",
                batch_values.len(),
                expected_batch
            ));
        }
        values.extend(batch_values);
    }
    let expected = prepared
        .len()
        .checked_mul(6)
        .ok_or_else(|| eyre!("governance participation result count overflows"))?;
    if values.len() != expected {
        return Err(eyre!(
            "governance participation multicall returned {}, expected {}",
            values.len(),
            expected
        ));
    }
    for (position, &(_row_index, account, sponsor_hint, voting_hint)) in prepared.iter().enumerate()
    {
        let start = position * 6;
        rows[_row_index] = Some(GovernanceParticipationRow {
            account,
            state: decode_governance_participation(
                proposal,
                voting_address,
                account,
                sponsor_hint,
                voting_hint,
                &values[start..start + 6],
            ),
        });
    }
    Ok(rows.into_iter().flatten().collect())
}

fn decode_governance_participation(
    proposal: &GovernanceProposal,
    voting_address: Address,
    account: Address,
    sponsor_hint: U256,
    voting_hint: U256,
    values: &[IMulticall3::Result],
) -> std::result::Result<GovernanceParticipation, GovernanceParticipationError> {
    if values.len() != 6 {
        return Err(GovernanceParticipationError::Decode {
            field: "row",
            reason: format!("expected 6 results, got {}", values.len()),
        });
    }
    let value = |index: usize, field: &'static str| {
        let result = &values[index];
        if result.success {
            Ok(&result.returnData)
        } else {
            Err(GovernanceParticipationError::Read {
                field,
                reason: "multicall target returned failure".into(),
            })
        }
    };
    let current_voting_power =
        <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_decode_validate(
            value(0, "voting power")?,
        )
        .map_err(|error| GovernanceParticipationError::Decode {
            field: "voting power",
            reason: error.to_string(),
        })?;
    let sponsorship_snapshot =
        GovernanceStaking::accountSnapshotAtCall::abi_decode_returns_validate(value(
            1,
            "sponsorship snapshot",
        )?)
        .map_err(|error| GovernanceParticipationError::Decode {
            field: "sponsorship snapshot",
            reason: error.to_string(),
        })?;
    if sponsorship_snapshot.interval < proposal.sponsor_snapshot_interval {
        return Err(GovernanceParticipationError::InvalidSnapshotHint(format!(
            "requested interval {}, received {}",
            proposal.sponsor_snapshot_interval, sponsorship_snapshot.interval
        )));
    }
    let voting_snapshot = GovernanceStaking::accountSnapshotAtCall::abi_decode_returns_validate(
        value(2, "voting snapshot")?,
    )
    .map_err(|error| GovernanceParticipationError::Decode {
        field: "voting snapshot",
        reason: error.to_string(),
    })?;
    if voting_snapshot.interval < proposal.voting_snapshot_interval {
        return Err(GovernanceParticipationError::InvalidSnapshotHint(format!(
            "requested interval {}, received {}",
            proposal.voting_snapshot_interval, voting_snapshot.interval
        )));
    }
    let sponsored =
        <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_decode_validate(
            value(3, "sponsored")?,
        )
        .map_err(|error| GovernanceParticipationError::Decode {
            field: "sponsored",
            reason: error.to_string(),
        })?;
    let voted =
        <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_decode_validate(
            value(4, "voted")?,
        )
        .map_err(|error| GovernanceParticipationError::Decode {
            field: "voted",
            reason: error.to_string(),
        })?;
    let last = GovernanceVoting::lastSponsoredCall::abi_decode_returns_validate(value(
        5,
        "last sponsored",
    )?)
    .map_err(|error| GovernanceParticipationError::Decode {
        field: "last sponsored",
        reason: error.to_string(),
    })?;
    Ok(GovernanceParticipation {
        proposal_version: proposal.contract_version,
        proposal_id: proposal.index,
        voting_contract: voting_address,
        account,
        current_voting_power,
        sponsorship_snapshot: GovernanceAccountSnapshot {
            interval: proposal.sponsor_snapshot_interval,
            voting_power: sponsorship_snapshot.votingPower,
            hint: sponsor_hint,
        },
        voting_snapshot: GovernanceAccountSnapshot {
            interval: proposal.voting_snapshot_interval,
            voting_power: voting_snapshot.votingPower,
            hint: voting_hint,
        },
        sponsored,
        voted,
        last_sponsored: GovernanceLastSponsored {
            last_sponsor_time: last.lastSponsorTime,
            proposal_id: last.proposalID,
        },
    })
}

async fn fetch_governance_multicall_raw(
    pool: &QueryRpcPool,
    multicall_address: Address,
    calls: Vec<(Address, Bytes)>,
) -> Result<Vec<IMulticall3::Result>> {
    let mut last_error = None;
    for _ in 0..pool.len() {
        let Some(handle) = pool.random_provider() else {
            break;
        };
        let mut multicall = handle
            .provider
            .multicall()
            .dynamic::<GovernanceStaking::votingPowerCall>()
            .address(multicall_address);
        for (target, call) in calls.iter().cloned() {
            multicall = multicall.add_call_dynamic(CallItem::new(target, call));
        }
        let request = multicall.to_try_aggregate_request(false);
        match timeout(GOVERNANCE_RPC_TIMEOUT, handle.provider.call(request)).await {
            Ok(Ok(output)) => {
                match IMulticall3::tryAggregateCall::abi_decode_returns_validate(&output) {
                    Ok(values) => return Ok(values),
                    Err(error) => {
                        pool.mark_bad_provider(&handle);
                        last_error = Some(eyre!(
                            "governance participation response decoding failed: {error}"
                        ));
                    }
                }
            }
            Ok(Err(error)) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("governance participation multicall failed: {error}"));
            }
            Err(_) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("governance participation multicall timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

/// Derive a proposal's complete lifecycle status from immutable on-chain fields, deployed rules,
/// and the latest chain timestamp.
///
/// Deadline arithmetic is checked so malformed or otherwise unrepresentable on-chain intervals
/// return an error rather than wrapping around to an earlier timestamp.
pub fn derive_governance_proposal_status(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    current_chain_time: U256,
) -> Result<GovernanceProposalStatus> {
    rules.validate()?;
    let sponsorship = proposal
        .publish_time
        .checked_add(rules.sponsor_window)
        .ok_or_else(|| eyre!("governance sponsorship deadline overflows U256"))?;
    let basis = match proposal.contract_version {
        GovernanceContractVersion::V2 => GovernanceQuorumBasis::AffirmativeOnly,
        GovernanceContractVersion::V1 => GovernanceQuorumBasis::TotalVotes,
    };
    let quorum_progress = match basis {
        GovernanceQuorumBasis::AffirmativeOnly => proposal.yay_votes,
        GovernanceQuorumBasis::TotalVotes => proposal
            .yay_votes
            .checked_add(proposal.nay_votes)
            .ok_or_else(|| eyre!("governance vote total overflows U256"))?,
    };
    let quorum_met = quorum_progress >= rules.quorum;
    let majority = match proposal.yay_votes.cmp(&proposal.nay_votes) {
        Ordering::Greater => GovernanceMajorityResult::Yay,
        Ordering::Less => GovernanceMajorityResult::Nay,
        Ordering::Equal => GovernanceMajorityResult::Tie,
    };
    let deadlines = if proposal.vote_call_time.is_zero() {
        GovernanceProposalDeadlines {
            sponsorship,
            voting_start: None,
            yay_end: None,
            nay_end: None,
            execution_start: None,
            execution_end: None,
        }
    } else {
        GovernanceProposalDeadlines {
            sponsorship,
            voting_start: Some(
                proposal
                    .vote_call_time
                    .checked_add(rules.voting_start_offset)
                    .ok_or_else(|| eyre!("governance voting start overflows U256"))?,
            ),
            yay_end: Some(
                proposal
                    .vote_call_time
                    .checked_add(rules.voting_yay_end_offset)
                    .ok_or_else(|| eyre!("governance yay deadline overflows U256"))?,
            ),
            nay_end: Some(
                proposal
                    .vote_call_time
                    .checked_add(rules.voting_nay_end_offset)
                    .ok_or_else(|| eyre!("governance nay deadline overflows U256"))?,
            ),
            execution_start: Some(
                proposal
                    .vote_call_time
                    .checked_add(rules.execution_start_offset)
                    .ok_or_else(|| eyre!("governance execution start overflows U256"))?,
            ),
            execution_end: Some(
                proposal
                    .vote_call_time
                    .checked_add(rules.execution_end_offset)
                    .ok_or_else(|| eyre!("governance execution deadline overflows U256"))?,
            ),
        }
    };
    let stage = if proposal.executed {
        GovernanceProposalStage::Executed
    } else if proposal.vote_call_time.is_zero() {
        if current_chain_time < sponsorship {
            if proposal.sponsorship < rules.sponsor_threshold {
                GovernanceProposalStage::AwaitingSponsorship
            } else {
                GovernanceProposalStage::ReadyToCallVote
            }
        } else if proposal.sponsorship < rules.sponsor_threshold {
            GovernanceProposalStage::SponsorshipExpired
        } else {
            GovernanceProposalStage::VoteCallExpired
        }
    } else {
        let voting_start = deadlines
            .voting_start
            .ok_or_else(|| eyre!("called proposal is missing voting start deadline"))?;
        let yay_end = deadlines
            .yay_end
            .ok_or_else(|| eyre!("called proposal is missing yay deadline"))?;
        let nay_end = deadlines
            .nay_end
            .ok_or_else(|| eyre!("called proposal is missing nay deadline"))?;
        let execution_start = deadlines
            .execution_start
            .ok_or_else(|| eyre!("called proposal is missing execution start deadline"))?;
        let execution_end = deadlines
            .execution_end
            .ok_or_else(|| eyre!("called proposal is missing execution deadline"))?;
        if current_chain_time <= voting_start {
            GovernanceProposalStage::VotingDelay
        } else if current_chain_time < yay_end {
            GovernanceProposalStage::VotingOpen
        } else if current_chain_time < nay_end {
            GovernanceProposalStage::NayOnlyVoting
        } else if !quorum_met || majority != GovernanceMajorityResult::Yay {
            GovernanceProposalStage::Failed
        } else if current_chain_time <= execution_start {
            GovernanceProposalStage::PassedAwaitingExecution
        } else if current_chain_time < execution_end {
            GovernanceProposalStage::PassedExecutable
        } else {
            GovernanceProposalStage::ExecutionExpired
        }
    };
    Ok(GovernanceProposalStatus {
        stage,
        deadlines,
        quorum_basis: basis,
        quorum: rules.quorum,
        quorum_progress,
        quorum_met,
        majority,
    })
}

pub fn derive_governance_proposal_stage(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    current_chain_time: U256,
) -> Result<GovernanceProposalStage> {
    Ok(derive_governance_proposal_status(proposal, rules, current_chain_time)?.stage)
}

/// Fetch proposal counts and all deployed lifecycle/quorum rules for the voting contracts.
/// Unsupported chains return `Ok(None)` without issuing an RPC request.
pub async fn fetch_governance_overview(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Option<GovernanceOverview>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let (query_rpc_pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut targets = vec![(GovernanceContractVersion::V2, contracts.voting)];
    if let Some(address) = contracts.voting_legacy {
        targets.push((GovernanceContractVersion::V1, address));
    }
    let summaries = fetch_contract_summaries(&query_rpc_pool, multicall_address, &targets).await?;
    let v2 = summaries[0].clone();
    let v1 = summaries.get(1).cloned();
    Ok(Some(GovernanceOverview { chain_id, v2, v1 }))
}

/// Fetch the latest block timestamp from a configured governance RPC.
pub async fn fetch_governance_chain_time(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<U256> {
    let (query_rpc_pool, _) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match timeout(
            GOVERNANCE_RPC_TIMEOUT,
            provider_handle
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        {
            Ok(Ok(Some(block))) => {
                return Ok(U256::from(block.header().timestamp()));
            }
            Ok(Ok(None)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance latest block was unavailable"));
            }
            Ok(Err(_)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance latest block RPC request failed"));
            }
            Err(_) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!(
                    "governance latest block RPC request timed out after {} milliseconds",
                    GOVERNANCE_RPC_TIMEOUT.as_millis()
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

/// Fetch one newest-first page from an overview.
///
/// The page is a single multicall, with V2 indices descending first and V1 indices descending
/// after the V2 history. A page that crosses the seam retains that order in its result.
pub async fn fetch_governance_page(
    overview: &GovernanceOverview,
    page: usize,
    page_size: NonZeroUsize,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Vec<GovernanceProposal>> {
    let v2_count = usize::try_from(overview.v2.proposal_count)
        .wrap_err("governance V2 proposal count exceeds platform page limits")?;
    let v1_count = overview
        .v1
        .as_ref()
        .map_or(Ok(0), |summary| usize::try_from(summary.proposal_count))
        .wrap_err("governance V1 proposal count exceeds platform page limits")?;
    let total_count = v2_count
        .checked_add(v1_count)
        .ok_or_else(|| eyre!("governance proposal count overflows platform page limits"))?;
    let offset = page
        .checked_mul(page_size.get())
        .ok_or_else(|| eyre!("governance page offset overflows platform page limits"))?;
    if offset >= total_count {
        return Ok(Vec::new());
    }
    let end = offset
        .checked_add(page_size.get())
        .ok_or_else(|| eyre!("governance page end overflows platform page limits"))?
        .min(total_count);

    let mut planned = Vec::with_capacity(end - offset);
    for global_index in offset..end {
        if global_index < v2_count {
            let index = v2_count - 1 - global_index;
            planned.push((
                GovernanceContractVersion::V2,
                overview.v2.address,
                U256::from(index),
            ));
        } else {
            let v1_index = global_index - v2_count;
            let summary = overview
                .v1
                .as_ref()
                .ok_or_else(|| eyre!("governance V1 page position has no V1 summary"))?;
            let index = v1_count - 1 - v1_index;
            planned.push((
                GovernanceContractVersion::V1,
                summary.address,
                U256::from(index),
            ));
        }
    }
    if planned.is_empty() {
        return Ok(Vec::new());
    }

    let (query_rpc_pool, multicall_address) =
        provider_for_chain(overview.chain_id, effective_chain, http)?;
    let proposals = fetch_proposal_calls(&query_rpc_pool, multicall_address, &planned).await?;
    for proposal in &proposals {
        let rules = match proposal.contract_version {
            GovernanceContractVersion::V2 => &overview.v2.rules,
            GovernanceContractVersion::V1 => {
                &overview
                    .v1
                    .as_ref()
                    .ok_or_else(|| eyre!("governance V1 rules are unavailable"))?
                    .rules
            }
        };
        derive_governance_proposal_status(proposal, rules, U256::ZERO).wrap_err_with(|| {
            format!(
                "governance {version:?} proposal arithmetic is invalid at index {index}",
                version = proposal.contract_version,
                index = proposal.index,
            )
        })?;
    }
    Ok(proposals)
}

fn provider_for_chain(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<(Arc<QueryRpcPool>, Address)> {
    let defaults = ChainConfigDefaults::for_chain(chain_id)
        .ok_or_else(|| eyre!("unsupported chain id {chain_id}"))?;
    let rpc_urls = effective_rpc_urls_for_chain(&defaults, effective_chain)?;
    let multicall_address = effective_chain
        .map(|chain| Address::from_str(&chain.multicall_contract))
        .transpose()
        .wrap_err("parse effective multicall contract")?
        .unwrap_or(defaults.multicall_contract);
    Ok((
        query_rpc_pool_with_http_client(rpc_urls, http),
        multicall_address,
    ))
}

async fn fetch_contract_summaries(
    query_rpc_pool: &QueryRpcPool,
    multicall_address: Address,
    targets: &[(GovernanceContractVersion, Address)],
) -> Result<Vec<GovernanceContractSummary>> {
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        let mut multicall = provider_handle
            .provider
            .multicall()
            .dynamic::<GovernanceVoting::proposalsLengthCall>()
            .address(multicall_address);
        for &(_, voting_address) in targets {
            for call in [
                GovernanceVoting::proposalsLengthCall {}.abi_encode(),
                GovernanceVoting::PROPOSAL_SPONSOR_THRESHOLDCall {}.abi_encode(),
                GovernanceVoting::QUORUMCall {}.abi_encode(),
                GovernanceVoting::SPONSOR_WINDOWCall {}.abi_encode(),
                GovernanceVoting::VOTING_START_OFFSETCall {}.abi_encode(),
                GovernanceVoting::VOTING_YAY_END_OFFSETCall {}.abi_encode(),
                GovernanceVoting::VOTING_NAY_END_OFFSETCall {}.abi_encode(),
                GovernanceVoting::EXECUTION_START_OFFSETCall {}.abi_encode(),
                GovernanceVoting::EXECUTION_END_OFFSETCall {}.abi_encode(),
            ] {
                multicall = multicall.add_call_dynamic(CallItem::new(voting_address, call.into()));
            }
        }
        let request = multicall.to_try_aggregate_request(false);
        match timeout(
            GOVERNANCE_RPC_TIMEOUT,
            provider_handle.provider.call(request),
        )
        .await
        {
            Ok(Ok(output)) => {
                let values =
                    match IMulticall3::tryAggregateCall::abi_decode_returns_validate(&output) {
                        Ok(values) => values,
                        Err(error) => {
                            query_rpc_pool.mark_bad_provider(&provider_handle);
                            last_error = Some(eyre!(
                                "governance metadata response decoding failed: {error}"
                            ));
                            continue;
                        }
                    };
                let expected = targets.len() * 9;
                if values.len() != expected {
                    query_rpc_pool.mark_bad_provider(&provider_handle);
                    last_error = Some(eyre!(
                        "governance metadata multicall returned {}, expected {}",
                        values.len(),
                        expected
                    ));
                    continue;
                }
                let mut summaries = Vec::with_capacity(targets.len());
                let mut failed = None;
                for (target_index, &(version, address)) in targets.iter().enumerate() {
                    let fields = &values[target_index * 9..target_index * 9 + 9];
                    let mut decoded = Vec::with_capacity(9);
                    for (field_index, field) in fields.iter().enumerate() {
                        if !field.success {
                            failed = Some(eyre!(
                                "governance {version:?} metadata call {field_index} failed"
                            ));
                            break;
                        }
                        match <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_decode_validate(&field.returnData) {
                            Ok(value) => decoded.push(value),
                            Err(error) => {
                                failed = Some(eyre!(
                                    "governance {version:?} metadata call {field_index} ABI decode failed: {error}"
                                ));
                                break;
                            }
                        }
                    }
                    if failed.is_some() {
                        break;
                    }
                    let rules = GovernanceContractRules {
                        sponsor_threshold: decoded[1],
                        quorum: decoded[2],
                        sponsor_window: decoded[3],
                        voting_start_offset: decoded[4],
                        voting_yay_end_offset: decoded[5],
                        voting_nay_end_offset: decoded[6],
                        execution_start_offset: decoded[7],
                        execution_end_offset: decoded[8],
                    };
                    if let Err(error) = rules.validate() {
                        failed = Some(error);
                        break;
                    }
                    summaries.push(GovernanceContractSummary {
                        version,
                        address,
                        proposal_count: decoded[0],
                        rules,
                    });
                }
                if let Some(error) = failed {
                    query_rpc_pool.mark_bad_provider(&provider_handle);
                    last_error = Some(error);
                    continue;
                }
                return Ok(summaries);
            }
            Ok(Err(error)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance metadata multicall failed: {error}"));
            }
            Err(_) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance metadata multicall timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

async fn fetch_proposal_calls(
    query_rpc_pool: &QueryRpcPool,
    multicall_address: Address,
    planned: &[(GovernanceContractVersion, Address, U256)],
) -> Result<Vec<GovernanceProposal>> {
    let expected_results = planned
        .len()
        .checked_mul(2)
        .ok_or_else(|| eyre!("governance proposal/action result count overflows"))?;
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        let mut multicall = provider_handle
            .provider
            .multicall()
            .dynamic::<GovernanceVoting::proposalsCall>()
            .address(multicall_address);
        for (_, address, index) in planned {
            multicall = multicall.add_call_dynamic(CallItem::new(
                *address,
                GovernanceVoting::proposalsCall { index: *index }
                    .abi_encode()
                    .into(),
            ));
            multicall = multicall.add_call_dynamic(CallItem::new(
                *address,
                GovernanceVoting::getActionsCall { id: *index }
                    .abi_encode()
                    .into(),
            ));
        }
        let request = multicall.to_try_aggregate_request(false);
        match timeout(
            GOVERNANCE_RPC_TIMEOUT,
            provider_handle.provider.call(request),
        )
        .await
        {
            Ok(Ok(output)) => {
                let Ok(values) =
                    IMulticall3::tryAggregateCall::abi_decode_returns_validate(&output)
                else {
                    query_rpc_pool.mark_bad_provider(&provider_handle);
                    last_error = Some(eyre!(
                        "governance proposal multicall response decoding failed"
                    ));
                    continue;
                };
                if values.len() != expected_results {
                    query_rpc_pool.mark_bad_provider(&provider_handle);
                    last_error = Some(eyre!(
                        "governance proposal/action multicall returned {} results, expected {}",
                        values.len(),
                        expected_results
                    ));
                    continue;
                }
                let mut proposals = Vec::with_capacity(planned.len());
                for (position, (version, address, index)) in planned.iter().enumerate() {
                    let proposal_result = &values[position * 2];
                    let action_result = &values[position * 2 + 1];
                    if !proposal_result.success {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!(
                            "governance {version:?} proposal call failed at index {index}"
                        ));
                        proposals.clear();
                        break;
                    }
                    let proposal = match version {
                        GovernanceContractVersion::V2 => {
                            GovernanceVotingV2::proposalsCall::abi_decode_returns_validate(
                                &proposal_result.returnData,
                            )
                            .map(|proposal| GovernanceProposal {
                                contract_version: *version,
                                index: *index,
                                contract_address: *address,
                                proposer: proposal.proposer,
                                proposal_document: proposal.proposalDocument,
                                publish_time: proposal.publishTime,
                                vote_call_time: proposal.voteCallTime,
                                sponsorship: proposal.sponsorship,
                                executed: proposal.executed,
                                yay_votes: proposal.yayVotes,
                                nay_votes: proposal.nayVotes,
                                sponsor_snapshot_interval: proposal.sponsorInterval,
                                voting_snapshot_interval: proposal.votingInterval,
                                actions: Vec::new(),
                            })
                            .map_err(|_| eyre!("ABI decoding failed"))
                        }
                        GovernanceContractVersion::V1 => {
                            GovernanceVoting::proposalsCall::abi_decode_returns_validate(
                                &proposal_result.returnData,
                            )
                            .map(|proposal| GovernanceProposal {
                                contract_version: *version,
                                index: *index,
                                contract_address: *address,
                                proposer: proposal.proposer,
                                proposal_document: proposal.proposalDocument,
                                publish_time: proposal.publishTime,
                                vote_call_time: proposal.voteCallTime,
                                sponsorship: proposal.sponsorship,
                                executed: proposal.executed,
                                yay_votes: proposal.yayVotes,
                                nay_votes: proposal.nayVotes,
                                sponsor_snapshot_interval: proposal.sponsorInterval,
                                voting_snapshot_interval: proposal.votingInterval,
                                actions: Vec::new(),
                            })
                            .map_err(|_| eyre!("ABI decoding failed"))
                        }
                    };
                    let Ok(proposal) = proposal else {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!(
                            "governance {version:?} proposal return decoding failed at index {index}"
                        ));
                        proposals.clear();
                        break;
                    };
                    if !action_result.success {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!(
                            "governance {version:?} getActions call failed at index {index}"
                        ));
                        proposals.clear();
                        break;
                    }
                    let actions =
                        match GovernanceVoting::getActionsCall::abi_decode_returns_validate(
                            &action_result.returnData,
                        ) {
                            Ok(actions) => actions
                                .into_iter()
                                .map(
                                    |(call_contract, calldata, value)| GovernanceProposalAction {
                                        call_contract,
                                        calldata,
                                        value,
                                    },
                                )
                                .collect(),
                            Err(error) => {
                                query_rpc_pool.mark_bad_provider(&provider_handle);
                                last_error = Some(eyre!(
                                    "governance {version:?} getActions return decoding failed at index {index}: {error}"
                                ));
                                proposals.clear();
                                break;
                            }
                        };
                    let proposal = GovernanceProposal {
                        actions,
                        ..proposal
                    };
                    proposals.push(proposal);
                }
                if proposals.len() == planned.len() {
                    return Ok(proposals);
                }
            }
            Ok(Err(error)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance proposal multicall failed: {error}"));
            }
            Err(_) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!(
                    "governance proposal multicall timed out after {} milliseconds",
                    GOVERNANCE_RPC_TIMEOUT.as_millis()
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::mpsc;
    use std::thread;

    use alloy::primitives::address;
    use alloy::providers::Failure;
    use serde_json::{Value, json};

    use super::*;

    const MULTICALL: Address = address!("0x0000000000000000000000000000000000000604");

    struct GovernanceFixture {
        url: String,
        calls: mpsc::Receiver<Vec<(Address, U256)>>,
        task: thread::JoinHandle<()>,
    }

    struct ParticipationFixture {
        url: String,
        calls: mpsc::Receiver<Vec<usize>>,
        shutdown: mpsc::Sender<()>,
        task: thread::JoinHandle<()>,
    }

    #[derive(Clone, Copy)]
    enum ActionFault {
        None,
        Failed,
        Malformed,
        WrongCount,
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Value {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).expect("read fixture request");
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let end = index + 4;
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .expect("content length");
                break (end, length);
            }
        };
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("read fixture body");
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .expect("fixture JSON")
    }

    fn spawn_governance_rpc_fixture(
        v2: Address,
        v1: Address,
        malformed_v1: bool,
        action_fault: ActionFault,
    ) -> GovernanceFixture {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fixture");
        let url = format!("http://{}", listener.local_addr().expect("fixture address"));
        let (calls_tx, calls_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            let mut page_calls = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                let request = read_request(&mut stream);
                let call_data = request["params"][0]
                    .get("input")
                    .or_else(|| request["params"][0].get("data"))
                    .and_then(Value::as_str)
                    .expect("eth_call data")
                    .parse::<alloy::primitives::Bytes>()
                    .expect("call bytes");
                let decoded = IMulticall3::tryAggregateCall::abi_decode(&call_data)
                    .expect("tryAggregate calldata");
                let page_batch = decoded.calls.iter().any(|call| {
                    call.callData
                        .starts_with(&GovernanceVoting::getActionsCall::SELECTOR)
                });
                if !page_batch {
                    assert_eq!(decoded.calls.len(), 18);
                }
                let target = request["params"][0]["to"]
                    .as_str()
                    .expect("multicall target")
                    .parse::<Address>()
                    .expect("multicall address");
                assert_eq!(target, MULTICALL);
                let returns = decoded
                    .calls
                    .into_iter()
                    .map(|call| {
                        let return_data = if call
                            .callData
                            .starts_with(&GovernanceVoting::proposalsLengthCall::SELECTOR)
                        {
                            assert!(call.target == v2 || call.target == v1);
                            let count = U256::from(2);
                            GovernanceVoting::proposalsLengthCall::abi_encode_returns(&count).into()
                        } else if call.callData.starts_with(
                            &GovernanceVoting::PROPOSAL_SPONSOR_THRESHOLDCall::SELECTOR,
                        ) {
                            GovernanceVoting::PROPOSAL_SPONSOR_THRESHOLDCall::abi_encode_returns(
                                &U256::from(77),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::QUORUMCall::SELECTOR)
                        {
                            GovernanceVoting::QUORUMCall::abi_encode_returns(&U256::from(50)).into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::SPONSOR_WINDOWCall::SELECTOR)
                        {
                            GovernanceVoting::SPONSOR_WINDOWCall::abi_encode_returns(&U256::from(
                                20,
                            ))
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::VOTING_START_OFFSETCall::SELECTOR)
                        {
                            GovernanceVoting::VOTING_START_OFFSETCall::abi_encode_returns(
                                &U256::from(5),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::VOTING_YAY_END_OFFSETCall::SELECTOR)
                        {
                            GovernanceVoting::VOTING_YAY_END_OFFSETCall::abi_encode_returns(
                                &U256::from(10),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::VOTING_NAY_END_OFFSETCall::SELECTOR)
                        {
                            GovernanceVoting::VOTING_NAY_END_OFFSETCall::abi_encode_returns(
                                &U256::from(15),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::EXECUTION_START_OFFSETCall::SELECTOR)
                        {
                            GovernanceVoting::EXECUTION_START_OFFSETCall::abi_encode_returns(
                                &U256::from(20),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::EXECUTION_END_OFFSETCall::SELECTOR)
                        {
                            GovernanceVoting::EXECUTION_END_OFFSETCall::abi_encode_returns(
                                &U256::from(30),
                            )
                            .into()
                        } else if call
                            .callData
                            .starts_with(&GovernanceVoting::getActionsCall::SELECTOR)
                        {
                            let decoded =
                                GovernanceVoting::getActionsCall::abi_decode(&call.callData)
                                    .expect("getActions calldata");
                            let marker = decoded.id.to::<u64>();
                            let marker_byte =
                                u8::try_from(marker).expect("fixture marker fits in an address");
                            let actions = if marker == 0 {
                                Vec::new()
                            } else if call.target == v2 {
                                vec![
                                    (
                                        Address::from([marker_byte; 20]),
                                        Bytes::from(vec![0xde, 0xad, marker_byte]),
                                        U256::ZERO,
                                    ),
                                    (
                                        Address::from([marker_byte.saturating_add(1); 20]),
                                        Bytes::new(),
                                        U256::from(7),
                                    ),
                                ]
                            } else {
                                assert_eq!(call.target, v1);
                                vec![(
                                    Address::from([marker_byte; 20]),
                                    Bytes::from(vec![0x01, 0x02]),
                                    U256::from(11),
                                )]
                            };
                            if matches!(action_fault, ActionFault::Malformed) && call.target == v2 {
                                Bytes::from(vec![0])
                            } else {
                                GovernanceVoting::getActionsCall::abi_encode_returns(&actions)
                                    .into()
                            }
                        } else {
                            let decoded =
                                GovernanceVoting::proposalsCall::abi_decode(&call.callData)
                                    .expect("proposal calldata");
                            page_calls.push((call.target, decoded.index));
                            let marker = decoded.index.to::<u64>();
                            let marker_byte =
                                u8::try_from(marker).expect("fixture marker fits in an address");
                            if call.target == v2 {
                                let proposal = GovernanceVotingV2::proposalsReturn {
                                    executed: marker % 2 == 0,
                                    proposer: Address::from([marker_byte; 20]),
                                    proposalDocument: format!("cid-{marker}"),
                                    publishTime: U256::from(100_u64 + marker),
                                    voteCallTime: U256::from(200_u64 + marker),
                                    sponsorship: U256::from(300_u64 + marker),
                                    yayVotes: U256::from(400_u64 + marker),
                                    nayVotes: U256::from(500_u64 + marker),
                                    sponsorInterval: U256::from(600_u64 + marker),
                                    votingInterval: U256::from(700_u64 + marker),
                                };
                                GovernanceVotingV2::proposalsCall::abi_encode_returns(&proposal)
                                    .into()
                            } else {
                                assert_eq!(call.target, v1);
                                let (yay_votes, nay_votes) = if malformed_v1 && marker == 1 {
                                    (U256::MAX, U256::from(1))
                                } else {
                                    (U256::from(400_u64 + marker), U256::from(500_u64 + marker))
                                };
                                let proposal = GovernanceVoting::proposalsReturn {
                                    proposer: Address::from([marker_byte; 20]),
                                    proposalDocument: format!("cid-{marker}"),
                                    publishTime: U256::from(100_u64 + marker),
                                    voteCallTime: U256::from(200_u64 + marker),
                                    sponsorship: U256::from(300_u64 + marker),
                                    executed: marker % 2 == 0,
                                    yayVotes: yay_votes,
                                    nayVotes: nay_votes,
                                    sponsorInterval: U256::from(600_u64 + marker),
                                    votingInterval: U256::from(700_u64 + marker),
                                };
                                GovernanceVoting::proposalsCall::abi_encode_returns(&proposal)
                                    .into()
                            }
                        };
                        IMulticall3::Result {
                            success: !(matches!(action_fault, ActionFault::Failed)
                                && call
                                    .callData
                                    .starts_with(&GovernanceVoting::getActionsCall::SELECTOR)
                                && call.target == v2),
                            returnData: return_data,
                        }
                    })
                    .collect::<Vec<_>>();
                let mut returns = returns;
                if matches!(action_fault, ActionFault::WrongCount) && page_batch {
                    returns.pop();
                }
                let response_data = IMulticall3::tryAggregateCall::abi_encode_returns(&returns);
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": format!("0x{}", alloy::hex::encode(response_data)),
                });
                let body = serde_json::to_string(&body).expect("serialize fixture response");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write fixture response");
            }
            calls_tx.send(page_calls).expect("send fixture calls");
        });
        GovernanceFixture {
            url,
            calls: calls_rx,
            task,
        }
    }

    fn spawn_governance_participation_fixture() -> ParticipationFixture {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fixture");
        let url = format!("http://{}", listener.local_addr().expect("fixture address"));
        let (calls_tx, calls_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            let serve = |mut stream: std::net::TcpStream,
                         expected_calls: usize,
                         returned_results: usize|
             -> usize {
                let request = read_request(&mut stream);
                let call_data = request["params"][0]
                    .get("input")
                    .or_else(|| request["params"][0].get("data"))
                    .and_then(Value::as_str)
                    .expect("eth_call data")
                    .parse::<alloy::primitives::Bytes>()
                    .expect("call bytes");
                let decoded = IMulticall3::tryAggregateCall::abi_decode(&call_data)
                    .expect("tryAggregate calldata");
                assert_eq!(decoded.calls.len(), expected_calls);
                let target = request["params"][0]["to"]
                    .as_str()
                    .expect("multicall target")
                    .parse::<Address>()
                    .expect("multicall address");
                assert_eq!(target, MULTICALL);
                let returns = if returned_results == expected_calls {
                    (0..returned_results)
                        .map(|_| IMulticall3::Result {
                            success: true,
                            returnData:
                                <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_encode(
                                    &U256::ZERO,
                                )
                                .into(),
                        })
                        .collect()
                } else {
                    let templates = encoded_participation_values();
                    (0..returned_results)
                        .map(|index| templates[index % templates.len()].clone())
                        .collect()
                };
                let response_data = IMulticall3::tryAggregateCall::abi_encode_returns(&returns);
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": format!("0x{}", alloy::hex::encode(response_data)),
                });
                let body = serde_json::to_string(&body).expect("serialize fixture response");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write fixture response");
                expected_calls
            };

            let mut call_counts = Vec::new();
            let (stream, _) = listener.accept().expect("accept snapshot request");
            call_counts.push(serve(stream, 11, 11));
            let (stream, _) = listener.accept().expect("accept participation request");
            call_counts.push(serve(stream, 60, 54));

            listener
                .set_nonblocking(true)
                .expect("set fixture listener nonblocking");
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        call_counts.push(serve(stream, 6, 12));
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        match shutdown_rx.recv_timeout(Duration::from_millis(10)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
                }
            }
            calls_tx.send(call_counts).expect("send fixture calls");
        });
        ParticipationFixture {
            url,
            calls: calls_rx,
            shutdown: shutdown_tx,
            task,
        }
    }

    #[tokio::test]
    async fn overview_and_page_decode_across_v2_v1_seam() {
        let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
        let GovernanceFixture { url, calls, task } = spawn_governance_rpc_fixture(
            contracts.voting,
            contracts.voting_legacy.expect("V1"),
            false,
            ActionFault::None,
        );
        let settings = crate::settings::WalletSettings::default();
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        let effective_chain = effective_chains.get_mut(&1).expect("Ethereum config");
        effective_chain.rpc_endpoints = vec![url];
        effective_chain.multicall_contract = MULTICALL.to_string();
        let http = HttpContext::direct_for_tests();

        let overview = fetch_governance_overview(1, Some(effective_chain), &http)
            .await
            .expect("overview request")
            .expect("Ethereum governance");
        assert_eq!(overview.v2.proposal_count, U256::from(2));
        assert_eq!(
            overview.v1.as_ref().expect("V1").rules.sponsor_threshold,
            U256::from(77)
        );

        let page = fetch_governance_page(
            &overview,
            0,
            NonZeroUsize::new(3).expect("nonzero page size"),
            Some(effective_chain),
            &http,
        )
        .await
        .expect("page request");
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].contract_version, GovernanceContractVersion::V2);
        assert_eq!(page[0].index, U256::from(1));
        assert_eq!(page[1].contract_version, GovernanceContractVersion::V2);
        assert_eq!(page[1].index, U256::ZERO);
        assert_eq!(page[0].proposer, Address::from([1; 20]));
        assert_eq!(page[0].proposal_document, "cid-1");
        assert_eq!(page[0].actions.len(), 2);
        assert_eq!(page[0].actions[0].call_contract, Address::from([1; 20]));
        assert_eq!(
            page[0].actions[0].calldata,
            Bytes::from(vec![0xde, 0xad, 1])
        );
        assert_eq!(page[0].actions[1].calldata, Bytes::new());
        assert_eq!(page[0].actions[1].value, U256::from(7));
        assert!(!page[0].executed);
        assert_eq!(page[1].proposer, Address::from([0; 20]));
        assert_eq!(page[1].proposal_document, "cid-0");
        assert!(page[1].actions.is_empty());
        assert!(page[1].executed);
        assert_eq!(page[2].contract_version, GovernanceContractVersion::V1);
        assert_eq!(page[2].index, U256::from(1));
        assert_eq!(page[2].proposal_document, "cid-1");
        assert_eq!(page[2].actions.len(), 1);
        assert_eq!(page[2].actions[0].value, U256::from(11));
        assert_eq!(page[2].publish_time, U256::from(101));
        assert_eq!(page[2].vote_call_time, U256::from(201));
        assert_eq!(page[2].sponsorship, U256::from(301));
        assert!(!page[2].executed);
        assert_eq!(page[2].yay_votes, U256::from(401));
        assert_eq!(page[2].nay_votes, U256::from(501));
        assert_eq!(page[2].sponsor_snapshot_interval, U256::from(601));
        assert_eq!(page[2].voting_snapshot_interval, U256::from(701));
        assert_eq!(
            calls.recv().expect("fixture calls"),
            vec![
                (contracts.voting, U256::from(1)),
                (contracts.voting, U256::ZERO),
                (contracts.voting_legacy.expect("V1"), U256::from(1))
            ]
        );
        task.join().expect("fixture task");
    }

    #[tokio::test]
    async fn page_rejects_v1_vote_total_overflow() {
        let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
        let GovernanceFixture { url, calls, task } = spawn_governance_rpc_fixture(
            contracts.voting,
            contracts.voting_legacy.expect("V1"),
            true,
            ActionFault::None,
        );
        let settings = crate::settings::WalletSettings::default();
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        let effective_chain = effective_chains.get_mut(&1).expect("Ethereum config");
        effective_chain.rpc_endpoints = vec![url];
        effective_chain.multicall_contract = MULTICALL.to_string();
        let http = HttpContext::direct_for_tests();

        let overview = fetch_governance_overview(1, Some(effective_chain), &http)
            .await
            .expect("overview request")
            .expect("Ethereum governance");
        let error = fetch_governance_page(
            &overview,
            0,
            NonZeroUsize::new(3).expect("nonzero page size"),
            Some(effective_chain),
            &http,
        )
        .await
        .expect_err("malformed page should be rejected");
        assert!(error.to_string().contains("arithmetic is invalid"));
        assert_eq!(
            calls.recv().expect("fixture calls"),
            vec![
                (contracts.voting, U256::from(1)),
                (contracts.voting, U256::ZERO),
                (contracts.voting_legacy.expect("V1"), U256::from(1))
            ]
        );
        task.join().expect("fixture task");
    }

    async fn action_fault_page_error(action_fault: ActionFault) -> String {
        let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
        let GovernanceFixture { url, calls, task } = spawn_governance_rpc_fixture(
            contracts.voting,
            contracts.voting_legacy.expect("V1"),
            false,
            action_fault,
        );
        let settings = crate::settings::WalletSettings::default();
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        let effective_chain = effective_chains.get_mut(&1).expect("Ethereum config");
        effective_chain.rpc_endpoints = vec![url];
        effective_chain.multicall_contract = MULTICALL.to_string();
        let http = HttpContext::direct_for_tests();
        let overview = fetch_governance_overview(1, Some(effective_chain), &http)
            .await
            .expect("overview request")
            .expect("Ethereum governance");
        let error = fetch_governance_page(
            &overview,
            0,
            NonZeroUsize::new(3).expect("nonzero page size"),
            Some(effective_chain),
            &http,
        )
        .await
        .expect_err("malformed action page should be rejected");
        let _ = calls.recv().expect("fixture calls");
        task.join().expect("fixture task");
        error.to_string()
    }

    #[tokio::test]
    async fn page_rejects_failed_malformed_and_wrong_count_actions() {
        let failed = action_fault_page_error(ActionFault::Failed).await;
        assert!(failed.contains("getActions call failed at index"));
        let malformed = action_fault_page_error(ActionFault::Malformed).await;
        assert!(malformed.contains("getActions return decoding failed at index"));
        let wrong_count = action_fault_page_error(ActionFault::WrongCount).await;
        assert!(wrong_count.contains("returned 5 results, expected 6"));
    }

    fn stage_test_proposal() -> GovernanceProposal {
        GovernanceProposal {
            contract_version: GovernanceContractVersion::V2,
            index: U256::ZERO,
            contract_address: Address::ZERO,
            proposer: Address::ZERO,
            proposal_document: String::new(),
            publish_time: U256::from(100_u64),
            vote_call_time: U256::ZERO,
            sponsorship: U256::ZERO,
            executed: false,
            yay_votes: U256::ZERO,
            nay_votes: U256::ZERO,
            sponsor_snapshot_interval: U256::from(999_u64),
            voting_snapshot_interval: U256::from(888_u64),
            actions: Vec::new(),
        }
    }

    fn stage_rules() -> GovernanceContractRules {
        GovernanceContractRules {
            sponsor_threshold: U256::from(10),
            quorum: U256::from(3),
            sponsor_window: U256::from(20),
            voting_start_offset: U256::from(5),
            voting_yay_end_offset: U256::from(10),
            voting_nay_end_offset: U256::from(15),
            execution_start_offset: U256::from(20),
            execution_end_offset: U256::from(30),
        }
    }

    #[test]
    fn stage_awaits_sponsorship_before_deadline() {
        let proposal = stage_test_proposal();
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(119_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::AwaitingSponsorship
        );
    }

    #[test]
    fn stage_expires_at_sponsorship_deadline() {
        let proposal = stage_test_proposal();
        let status =
            derive_governance_proposal_status(&proposal, &stage_rules(), U256::from(120_u64))
                .expect("status derivation");
        assert_eq!(status.deadlines.sponsorship, U256::from(120));
        assert_eq!(status.stage, GovernanceProposalStage::SponsorshipExpired);
    }

    #[test]
    fn threshold_reached_without_vote_call_is_not_expired() {
        let mut proposal = stage_test_proposal();
        proposal.sponsorship = U256::from(10_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(120_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::VoteCallExpired
        );
    }

    #[test]
    fn stage_is_voting_open_immediately_before_voting_end() {
        let mut proposal = stage_test_proposal();
        proposal.vote_call_time = U256::from(200_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(209_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::VotingOpen
        );
    }

    #[test]
    fn strict_stage_boundaries_are_protocol_accurate() {
        let mut proposal = stage_test_proposal();
        proposal.sponsorship = U256::from(10);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(119)).unwrap(),
            GovernanceProposalStage::ReadyToCallVote
        );
        proposal.vote_call_time = U256::from(200);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(205)).unwrap(),
            GovernanceProposalStage::VotingDelay
        );
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(210)).unwrap(),
            GovernanceProposalStage::NayOnlyVoting
        );
        proposal.yay_votes = U256::from(4);
        proposal.nay_votes = U256::from(1);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(220)).unwrap(),
            GovernanceProposalStage::PassedAwaitingExecution
        );
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(221)).unwrap(),
            GovernanceProposalStage::PassedExecutable
        );
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(230)).unwrap(),
            GovernanceProposalStage::ExecutionExpired
        );
    }

    #[test]
    fn quorum_basis_is_version_specific_and_checked() {
        let mut v2 = stage_test_proposal();
        v2.vote_call_time = U256::from(200);
        v2.yay_votes = U256::from(2);
        v2.nay_votes = U256::from(1);
        let v2_status =
            derive_governance_proposal_status(&v2, &stage_rules(), U256::from(215)).unwrap();
        assert_eq!(
            v2_status.quorum_basis,
            GovernanceQuorumBasis::AffirmativeOnly
        );
        assert_eq!(v2_status.quorum_progress, U256::from(2));
        assert!(!v2_status.quorum_met);

        let mut v1 = v2;
        v1.contract_version = GovernanceContractVersion::V1;
        v1.yay_votes = U256::from(2);
        v1.nay_votes = U256::from(1);
        let status =
            derive_governance_proposal_status(&v1, &stage_rules(), U256::from(215)).unwrap();
        assert_eq!(status.quorum_basis, GovernanceQuorumBasis::TotalVotes);
        assert_eq!(status.quorum_progress, U256::from(3));
        assert!(status.quorum_met);
        assert_eq!(status.majority, GovernanceMajorityResult::Yay);
    }

    #[test]
    fn invalid_rules_and_vote_total_overflow_are_errors() {
        let proposal = stage_test_proposal();
        let mut invalid = stage_rules();
        invalid.execution_start_offset = invalid.voting_yay_end_offset;
        assert!(derive_governance_proposal_status(&proposal, &invalid, U256::ZERO).is_err());
        let mut overflow = stage_test_proposal();
        overflow.contract_version = GovernanceContractVersion::V1;
        overflow.yay_votes = U256::MAX;
        overflow.nay_votes = U256::from(1);
        assert!(derive_governance_proposal_status(&overflow, &stage_rules(), U256::ZERO).is_err());
    }

    #[test]
    fn stage_outcome_is_resolved_at_voting_end_and_ties_fail() {
        let mut proposal = stage_test_proposal();
        proposal.vote_call_time = U256::from(200_u64);
        proposal.yay_votes = U256::from(3_u64);
        proposal.nay_votes = U256::from(1_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(215_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::PassedAwaitingExecution
        );

        proposal.yay_votes = U256::from(3_u64);
        proposal.nay_votes = U256::from(4_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(215_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::Failed
        );

        proposal.yay_votes = U256::from(3_u64);
        proposal.nay_votes = U256::from(3_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(215_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::Failed
        );
    }

    #[test]
    fn executed_stage_takes_precedence() {
        let mut proposal = stage_test_proposal();
        proposal.executed = true;
        proposal.vote_call_time = U256::from(200_u64);
        assert_eq!(
            derive_governance_proposal_stage(&proposal, &stage_rules(), U256::from(205_u64))
                .expect("stage derivation"),
            GovernanceProposalStage::Executed
        );
    }

    fn participation_proposal(version: GovernanceContractVersion) -> GovernanceProposal {
        let contracts = governance_contracts(1).expect("Ethereum governance contracts");
        GovernanceProposal {
            contract_version: version,
            index: U256::from(7),
            contract_address: match version {
                GovernanceContractVersion::V2 => contracts.voting,
                GovernanceContractVersion::V1 => contracts.voting_legacy.expect("legacy voting"),
            },
            proposer: Address::from([1_u8; 20]),
            proposal_document: String::new(),
            publish_time: U256::from(100),
            vote_call_time: U256::from(200),
            sponsorship: U256::ZERO,
            executed: false,
            yay_votes: U256::ZERO,
            nay_votes: U256::ZERO,
            sponsor_snapshot_interval: U256::from(8),
            voting_snapshot_interval: U256::from(9),
            actions: Vec::new(),
        }
    }

    fn encoded_participation_values() -> Vec<IMulticall3::Result> {
        let encoded: Vec<std::result::Result<Bytes, Failure>> = vec![
            Ok(
                <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_encode(
                    &U256::from(40),
                )
                .into(),
            ),
            Ok(
                GovernanceStaking::accountSnapshotAtCall::abi_encode_returns(
                    &GovernanceStaking::accountSnapshotAtReturn {
                        interval: U256::from(8),
                        votingPower: U256::from(15),
                    },
                )
                .into(),
            ),
            Ok(
                GovernanceStaking::accountSnapshotAtCall::abi_encode_returns(
                    &GovernanceStaking::accountSnapshotAtReturn {
                        interval: U256::from(9),
                        votingPower: U256::from(22),
                    },
                )
                .into(),
            ),
            Ok(
                <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_encode(
                    &U256::from(3),
                )
                .into(),
            ),
            Ok(
                <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_encode(
                    &U256::from(5),
                )
                .into(),
            ),
            Ok(GovernanceVoting::lastSponsoredCall::abi_encode_returns(
                &GovernanceVoting::lastSponsoredReturn {
                    lastSponsorTime: U256::from(10),
                    proposalID: U256::from(6),
                },
            )
            .into()),
        ];
        encoded
            .into_iter()
            .map(|result| IMulticall3::Result {
                success: result.is_ok(),
                returnData: result.unwrap_or_default(),
            })
            .collect()
    }

    #[tokio::test]
    async fn participation_rejects_short_first_batch_before_fetching_next_batch() {
        let fixture = spawn_governance_participation_fixture();
        let settings = crate::settings::WalletSettings::default();
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        let effective_chain = effective_chains.get_mut(&1).expect("Ethereum config");
        effective_chain.rpc_endpoints = vec![fixture.url.clone()];
        effective_chain.multicall_contract = MULTICALL.to_string();
        let accounts: Vec<_> = (1_u8..=11)
            .map(|marker| Address::from([marker; 20]))
            .collect();
        let http = HttpContext::direct_for_tests();
        let error = fetch_governance_participation(
            1,
            &participation_proposal(GovernanceContractVersion::V2),
            &accounts,
            Some(effective_chain),
            &http,
        )
        .await
        .expect_err("short participation batch should be rejected");
        assert!(error.to_string().contains("returned 54, expected 60"));

        fixture.shutdown.send(()).expect("stop fixture");
        let call_counts = fixture.calls.recv().expect("fixture calls");
        assert_eq!(call_counts, vec![11, 60]);
        fixture.task.join().expect("fixture task");
    }

    #[test]
    fn participation_decode_preserves_v1_and_v2_identity_routing() {
        let account = Address::from([3_u8; 20]);
        for version in [GovernanceContractVersion::V1, GovernanceContractVersion::V2] {
            let proposal = participation_proposal(version);
            let row = decode_governance_participation(
                &proposal,
                proposal.contract_address,
                account,
                U256::from(1),
                U256::from(2),
                &encoded_participation_values(),
            )
            .expect("participation decode");
            assert_eq!(row.proposal_version, version);
            assert_eq!(row.voting_contract, proposal.contract_address);
            assert_eq!(row.current_voting_power, U256::from(40));
            assert_eq!(row.sponsorship_snapshot.voting_power, U256::from(15));
            assert_eq!(row.voting_snapshot.voting_power, U256::from(22));
            assert_eq!(row.sponsored, U256::from(3));
            assert_eq!(row.voted, U256::from(5));
            assert_eq!(row.last_sponsored.proposal_id, U256::from(6));
        }
    }

    #[test]
    fn participation_decode_accepts_later_snapshot_boundaries_but_rejects_earlier() {
        let proposal = participation_proposal(GovernanceContractVersion::V2);
        let account = Address::from([3_u8; 20]);
        let mut values = encoded_participation_values();
        values[1].returnData = GovernanceStaking::accountSnapshotAtCall::abi_encode_returns(
            &GovernanceStaking::accountSnapshotAtReturn {
                interval: U256::from(10),
                votingPower: U256::from(17),
            },
        )
        .into();
        values[2].returnData = GovernanceStaking::accountSnapshotAtCall::abi_encode_returns(
            &GovernanceStaking::accountSnapshotAtReturn {
                interval: U256::from(12),
                votingPower: U256::from(24),
            },
        )
        .into();
        let row = decode_governance_participation(
            &proposal,
            proposal.contract_address,
            account,
            U256::from(4),
            U256::from(5),
            &values,
        )
        .expect("later snapshot boundaries should decode");
        assert_eq!(
            row.sponsorship_snapshot.interval,
            proposal.sponsor_snapshot_interval
        );
        assert_eq!(row.sponsorship_snapshot.voting_power, U256::from(17));
        assert_eq!(row.sponsorship_snapshot.hint, U256::from(4));
        assert_eq!(
            row.voting_snapshot.interval,
            proposal.voting_snapshot_interval
        );
        assert_eq!(row.voting_snapshot.voting_power, U256::from(24));
        assert_eq!(row.voting_snapshot.hint, U256::from(5));

        values[1].returnData = GovernanceStaking::accountSnapshotAtCall::abi_encode_returns(
            &GovernanceStaking::accountSnapshotAtReturn {
                interval: U256::from(7),
                votingPower: U256::from(17),
            },
        )
        .into();
        let error = decode_governance_participation(
            &proposal,
            proposal.contract_address,
            account,
            U256::from(4),
            U256::from(5),
            &values,
        )
        .expect_err("an earlier snapshot boundary should be rejected");
        assert!(matches!(
            error,
            GovernanceParticipationError::InvalidSnapshotHint(_)
        ));
    }

    #[test]
    fn participation_decode_keeps_allow_failure_field_errors_local() {
        let proposal = participation_proposal(GovernanceContractVersion::V2);
        let account = Address::from([3_u8; 20]);
        let mut values = encoded_participation_values();
        values[4].success = false;
        let error = decode_governance_participation(
            &proposal,
            proposal.contract_address,
            account,
            U256::from(1),
            U256::from(2),
            &values,
        )
        .expect_err("failed voted field should isolate the row");
        assert!(matches!(
            error,
            GovernanceParticipationError::Read { field: "voted", .. }
        ));
    }

    #[test]
    fn capacity_distinguishes_unavailable_exhausted_and_inconsistent_data() {
        let snapshot = GovernanceAccountSnapshot {
            interval: U256::from(8),
            voting_power: U256::from(10),
            hint: U256::from(2),
        };
        assert_eq!(
            calculate_governance_capacity(Some(&snapshot), U256::from(4))
                .expect("remaining capacity")
                .remaining,
            Some(U256::from(6))
        );
        assert_eq!(
            calculate_governance_capacity(Some(&snapshot), U256::from(10))
                .expect("exhausted capacity")
                .remaining,
            Some(U256::ZERO)
        );
        assert_eq!(
            calculate_governance_capacity(None, U256::ZERO),
            Err(GovernanceCapacityError::SnapshotUnavailable)
        );
        assert_eq!(
            calculate_governance_capacity(Some(&snapshot), U256::from(11)),
            Err(GovernanceCapacityError::AllocatedExceedsSnapshot)
        );
    }

    #[test]
    fn governance_calls_route_to_both_versions_with_zero_native_value() {
        let account = Address::from([4_u8; 20]);
        for version in [GovernanceContractVersion::V1, GovernanceContractVersion::V2] {
            let sponsor = build_sponsor_call(
                1,
                version,
                U256::from(7),
                U256::from(3),
                account,
                U256::from(2),
            )
            .expect("sponsor call");
            assert_eq!(sponsor.value, U256::ZERO);
            assert_eq!(sponsor.to, participation_proposal(version).contract_address);
            let decoded = GovernanceVoting::sponsorProposalCall::abi_decode(&sponsor.data)
                .expect("sponsor calldata");
            assert_eq!(decoded.proposalID, U256::from(7));
            assert_eq!(decoded.amount, U256::from(3));
            assert_eq!(decoded.account, account);
            assert_eq!(decoded.hint, U256::from(2));

            let unsponsor = build_unsponsor_call(1, version, U256::from(7), U256::from(3), account)
                .expect("unsponsor call");
            assert_eq!(unsponsor.value, U256::ZERO);
            assert_eq!(
                unsponsor.to,
                participation_proposal(version).contract_address
            );
            let decoded = GovernanceVoting::unsponsorProposalCall::abi_decode(&unsponsor.data)
                .expect("unsponsor calldata");
            assert_eq!(decoded.proposalID, U256::from(7));
            assert_eq!(decoded.amount, U256::from(3));
            assert_eq!(decoded.account, account);

            let call_vote =
                build_call_vote_call(1, version, U256::from(7), account).expect("call vote");
            assert_eq!(call_vote.value, U256::ZERO);
            let decoded = GovernanceVoting::callVoteCall::abi_decode(&call_vote.data)
                .expect("call vote calldata");
            assert_eq!(decoded.proposalID, U256::from(7));

            for yay in [true, false] {
                let vote = build_vote_call(
                    1,
                    version,
                    U256::from(7),
                    U256::from(6),
                    yay,
                    account,
                    U256::from(2),
                )
                .expect("vote call");
                assert_eq!(vote.value, U256::ZERO);
                assert_eq!(vote.to, participation_proposal(version).contract_address);
                let decoded =
                    GovernanceVoting::voteCall::abi_decode(&vote.data).expect("vote calldata");
                assert_eq!(decoded.proposalID, U256::from(7));
                assert_eq!(decoded.amount, U256::from(6));
                assert_eq!(decoded.yay, yay);
                assert_eq!(decoded.account, account);
                assert_eq!(decoded.hint, U256::from(2));
            }
        }
        assert_eq!(
            build_sponsor_call(
                1,
                GovernanceContractVersion::V2,
                U256::ZERO,
                U256::ZERO,
                account,
                U256::ZERO,
            ),
            Err(GovernanceCallError::ZeroAmount)
        );
    }

    #[test]
    fn guards_follow_strict_protocol_boundaries_and_chain_time() {
        let mut proposal = participation_proposal(GovernanceContractVersion::V2);
        let rules = stage_rules();
        proposal.publish_time = U256::from(100);
        proposal.vote_call_time = U256::ZERO;
        assert!(
            guard_sponsor(
                &proposal,
                &rules,
                Some(U256::from(119)),
                U256::from(1),
                None
            )
            .is_ok()
        );
        assert_eq!(
            guard_sponsor(
                &proposal,
                &rules,
                Some(U256::from(120)),
                U256::from(1),
                None
            ),
            Err(GovernanceGuardError::WrongStage)
        );
        let mut cooldown_proposal = proposal.clone();
        cooldown_proposal.publish_time = U256::from(1_000_000);
        cooldown_proposal.sponsorship = U256::ZERO;
        let cooldown_now = U256::from(1_000_019);
        let equal_last_time = cooldown_now - U256::from(GOVERNANCE_SPONSOR_LOCKOUT_TIME_SECONDS);
        assert_eq!(
            guard_sponsor(
                &cooldown_proposal,
                &rules,
                Some(cooldown_now),
                U256::from(1),
                Some(&GovernanceLastSponsored {
                    last_sponsor_time: equal_last_time,
                    proposal_id: U256::from(3),
                }),
            ),
            Err(GovernanceGuardError::SponsorshipCooldown)
        );
        assert!(
            guard_sponsor(
                &cooldown_proposal,
                &rules,
                Some(cooldown_now),
                U256::from(1),
                Some(&GovernanceLastSponsored {
                    last_sponsor_time: equal_last_time - U256::from(1),
                    proposal_id: U256::from(3),
                }),
            )
            .is_ok()
        );
        assert_eq!(
            guard_sponsor(&proposal, &rules, None, U256::from(1), None),
            Err(GovernanceGuardError::ChainTimeUnavailable)
        );

        proposal.sponsorship = U256::from(10);
        proposal.vote_call_time = U256::ZERO;
        assert!(guard_call_vote(&proposal, &rules, Some(U256::from(119))).is_ok());
        proposal.vote_call_time = U256::from(200);
        assert_eq!(
            guard_yay_vote(&proposal, &rules, Some(U256::from(205)), U256::from(1)),
            Err(GovernanceGuardError::WrongStage)
        );
        assert_eq!(
            guard_yay_vote(&proposal, &rules, Some(U256::from(210)), U256::from(1)),
            Err(GovernanceGuardError::WrongStage)
        );
        assert!(guard_yay_vote(&proposal, &rules, Some(U256::from(209)), U256::from(1)).is_ok());
        assert!(guard_nay_vote(&proposal, &rules, Some(U256::from(210)), U256::from(1)).is_ok());
        assert_eq!(
            guard_nay_vote(&proposal, &rules, Some(U256::from(215)), U256::from(1)),
            Err(GovernanceGuardError::WrongStage)
        );
    }
}
