//! Read-only enumeration of the RAILGUN governance voting contracts.

use std::cmp::Ordering;
use std::num::NonZeroUsize;

use alloy::consensus::BlockHeader as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};
use railgun_ui::governance_contracts;
use thiserror::Error;

use crate::rpc_broker::total_failure;
use crate::staking::{
    MulticallChunkSize, fetch_account_snapshots, fetch_latest_block, snapshot_hint,
};
use crate::{ClassRoute, HttpContext, RpcBrokerError, RpcRoute, WalletRpcOrigin};

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

/// Fetch one account's proposal state. Snapshot discovery reads the configured Staking contract
/// first, then one multicall reads Staking's current and historical power plus the proposal's
/// version-specific Voting contract state. A snapshot failure becomes the row's error state.
pub async fn fetch_governance_participation(
    chain_id: u64,
    proposal: &GovernanceProposal,
    account: Address,
    route: &ClassRoute,
    http: &HttpContext,
) -> Result<GovernanceParticipationRow> {
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

    let snapshots = match fetch_account_snapshots(
        chain_id,
        account,
        route,
        http,
        MulticallChunkSize::default(),
    )
    .await
    {
        Ok(snapshots) => snapshots,
        Err(error) => {
            return Ok(GovernanceParticipationRow {
                account,
                state: Err(GovernanceParticipationError::SnapshotUnavailable(
                    error.to_string(),
                )),
            });
        }
    };
    let hints =
        snapshot_hint(&snapshots, proposal.sponsor_snapshot_interval).and_then(|sponsor_hint| {
            snapshot_hint(&snapshots, proposal.voting_snapshot_interval)
                .map(|voting_hint| (sponsor_hint, voting_hint))
        });
    let (sponsor_hint, voting_hint) = match hints {
        Ok(hints) => hints,
        Err(error) => {
            return Ok(GovernanceParticipationRow {
                account,
                state: Err(GovernanceParticipationError::InvalidSnapshotHint(
                    error.to_string(),
                )),
            });
        }
    };

    let calls = vec![
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
    ];
    let values =
        fetch_governance_multicall_raw(route, http, calls, "governance participation").await?;
    Ok(GovernanceParticipationRow {
        account,
        state: decode_governance_participation(
            proposal,
            voting_address,
            account,
            sponsor_hint,
            voting_hint,
            &values,
        ),
    })
}

fn decode_governance_participation(
    proposal: &GovernanceProposal,
    voting_address: Address,
    account: Address,
    sponsor_hint: U256,
    voting_hint: U256,
    values: &[std::result::Result<Bytes, RpcBrokerError>],
) -> std::result::Result<GovernanceParticipation, GovernanceParticipationError> {
    if values.len() != 6 {
        return Err(GovernanceParticipationError::Decode {
            field: "row",
            reason: format!("expected 6 results, got {}", values.len()),
        });
    }
    let value = |index: usize, field: &'static str| {
        values[index]
            .as_ref()
            .map_err(|error| GovernanceParticipationError::Read {
                field,
                reason: error.to_string(),
            })
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

/// Submits one broker batch and returns the per-call results in input order.
///
/// A member revert is reported to the caller as `Err(RpcBrokerError::InnerRevert(_))` so it can be
/// distinguished from a transport or JSON-RPC failure; only a whole-batch failure is an outer
/// error.
async fn fetch_governance_multicall_raw(
    route: &ClassRoute,
    http: &HttpContext,
    calls: Vec<(Address, Bytes)>,
    caller: &'static str,
) -> Result<Vec<std::result::Result<Bytes, RpcBrokerError>>> {
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            RpcRoute::from(route.clone()),
            calls,
            WalletRpcOrigin::Governance.into(),
        )
        .await
        .map_err(|error| eyre!("{caller} multicall failed: {error}"))?;
    if let Some(error) = total_failure(&results) {
        return Err(eyre!("{caller} multicall failed: {error}"));
    }
    Ok(results)
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
    route: &ClassRoute,
    http: &HttpContext,
) -> Result<Option<GovernanceOverview>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let mut targets = vec![(GovernanceContractVersion::V2, contracts.voting)];
    if let Some(address) = contracts.voting_legacy {
        targets.push((GovernanceContractVersion::V1, address));
    }
    let summaries = fetch_contract_summaries(route, http, &targets).await?;
    let v2 = summaries[0].clone();
    let v1 = summaries.get(1).cloned();
    Ok(Some(GovernanceOverview { chain_id, v2, v1 }))
}

/// Fetch the latest block timestamp over `route`.
pub async fn fetch_governance_chain_time(route: &ClassRoute, http: &HttpContext) -> Result<U256> {
    Ok(U256::from(
        fetch_latest_block(route, http, WalletRpcOrigin::Governance)
            .await?
            .header
            .timestamp(),
    ))
}

/// Fetch one newest-first page from an overview.
///
/// The page is a single multicall, with V2 indices descending first and V1 indices descending
/// after the V2 history. A page that crosses the seam retains that order in its result.
pub async fn fetch_governance_page(
    overview: &GovernanceOverview,
    page: usize,
    page_size: NonZeroUsize,
    route: &ClassRoute,
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

    let proposals = fetch_proposal_calls(route, http, &planned).await?;
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

async fn fetch_contract_summaries(
    route: &ClassRoute,
    http: &HttpContext,
    targets: &[(GovernanceContractVersion, Address)],
) -> Result<Vec<GovernanceContractSummary>> {
    let calls = targets
        .iter()
        .flat_map(|&(_, voting_address)| {
            [
                GovernanceVoting::proposalsLengthCall {}.abi_encode(),
                GovernanceVoting::PROPOSAL_SPONSOR_THRESHOLDCall {}.abi_encode(),
                GovernanceVoting::QUORUMCall {}.abi_encode(),
                GovernanceVoting::SPONSOR_WINDOWCall {}.abi_encode(),
                GovernanceVoting::VOTING_START_OFFSETCall {}.abi_encode(),
                GovernanceVoting::VOTING_YAY_END_OFFSETCall {}.abi_encode(),
                GovernanceVoting::VOTING_NAY_END_OFFSETCall {}.abi_encode(),
                GovernanceVoting::EXECUTION_START_OFFSETCall {}.abi_encode(),
                GovernanceVoting::EXECUTION_END_OFFSETCall {}.abi_encode(),
            ]
            .into_iter()
            .map(move |call| (voting_address, call.into()))
        })
        .collect();
    let values = fetch_governance_multicall_raw(route, http, calls, "governance overview").await?;
    let mut summaries = Vec::with_capacity(targets.len());
    for (target_index, &(version, address)) in targets.iter().enumerate() {
        let fields = &values[target_index * 9..target_index * 9 + 9];
        let mut decoded = Vec::with_capacity(9);
        for (field_index, field) in fields.iter().enumerate() {
            let Ok(return_data) = field else {
                return Err(eyre!(
                    "governance {version:?} metadata call {field_index} failed"
                ));
            };
            match <alloy::sol_types::sol_data::Uint<256> as alloy::sol_types::SolType>::abi_decode_validate(return_data) {
                Ok(value) => decoded.push(value),
                Err(error) => {
                    return Err(eyre!(
                        "governance {version:?} metadata call {field_index} ABI decode failed: {error}"
                    ));
                }
            }
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
        rules.validate()?;
        summaries.push(GovernanceContractSummary {
            version,
            address,
            proposal_count: decoded[0],
            rules,
        });
    }
    Ok(summaries)
}

async fn fetch_proposal_calls(
    route: &ClassRoute,
    http: &HttpContext,
    planned: &[(GovernanceContractVersion, Address, U256)],
) -> Result<Vec<GovernanceProposal>> {
    let calls = planned
        .iter()
        .flat_map(|(_, address, index)| {
            [
                (
                    *address,
                    GovernanceVoting::proposalsCall { index: *index }
                        .abi_encode()
                        .into(),
                ),
                (
                    *address,
                    GovernanceVoting::getActionsCall { id: *index }
                        .abi_encode()
                        .into(),
                ),
            ]
        })
        .collect();
    let values = fetch_governance_multicall_raw(route, http, calls, "governance proposal").await?;
    let mut proposals = Vec::with_capacity(planned.len());
    for (position, (version, address, index)) in planned.iter().enumerate() {
        let proposal_result = &values[position * 2];
        let action_result = &values[position * 2 + 1];
        let Ok(proposal_return_data) = proposal_result else {
            return Err(eyre!(
                "governance {version:?} proposal call failed at index {index}"
            ));
        };
        // Anyone can store any bytes as a proposal document, so these decodes skip validation:
        // alloy then replaces invalid UTF-8 instead of failing the whole page.
        let proposal = match version {
            GovernanceContractVersion::V2 => {
                GovernanceVotingV2::proposalsCall::abi_decode_returns(proposal_return_data)
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
                GovernanceVoting::proposalsCall::abi_decode_returns(proposal_return_data)
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
        }
        .map_err(|_| {
            eyre!("governance {version:?} proposal return decoding failed at index {index}")
        })?;
        let Ok(action_return_data) = action_result else {
            return Err(eyre!(
                "governance {version:?} getActions call failed at index {index}"
            ));
        };
        let actions = GovernanceVoting::getActionsCall::abi_decode_returns_validate(
            action_return_data,
        )
        .map_err(|error| {
            eyre!(
                "governance {version:?} getActions return decoding failed at index {index}: {error}"
            )
        })?
        .into_iter()
        .map(
            |(call_contract, calldata, value)| GovernanceProposalAction {
                call_contract,
                calldata,
                value,
            },
        )
        .collect();
        proposals.push(GovernanceProposal {
            actions,
            ..proposal
        });
    }
    Ok(proposals)
}

#[cfg(test)]
mod tests;
