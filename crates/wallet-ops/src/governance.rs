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
use tokio::time::timeout;

use crate::settings::EffectiveChainConfig;
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
    let v2 = fetch_contract_summary(
        &query_rpc_pool,
        multicall_address,
        contracts.voting,
        GovernanceContractVersion::V2,
    )
    .await?;
    let v1 = match contracts.voting_legacy {
        Some(address) => Some(
            fetch_contract_summary(
                &query_rpc_pool,
                multicall_address,
                address,
                GovernanceContractVersion::V1,
            )
            .await?,
        ),
        None => None,
    };
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

async fn fetch_contract_summary(
    query_rpc_pool: &QueryRpcPool,
    multicall_address: Address,
    voting_address: Address,
    version: GovernanceContractVersion,
) -> Result<GovernanceContractSummary> {
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
        multicall = multicall.add_call_dynamic(CallItem::new(
            voting_address,
            GovernanceVoting::proposalsLengthCall {}.abi_encode().into(),
        ));
        multicall = multicall.add_call_dynamic(CallItem::new(
            voting_address,
            GovernanceVoting::PROPOSAL_SPONSOR_THRESHOLDCall {}
                .abi_encode()
                .into(),
        ));
        for call in [
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
        match timeout(GOVERNANCE_RPC_TIMEOUT, multicall.try_aggregate(false)).await {
            Ok(Ok(values)) if values.len() == 9 => {
                let mut values = values.into_iter();
                let proposal_count = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("proposalsLength call failed: {error}"));
                        continue;
                    }
                };
                let sponsor_threshold = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("sponsor threshold call failed: {error}"));
                        continue;
                    }
                };
                let quorum = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("quorum call failed: {error}"));
                        continue;
                    }
                };
                let sponsor_window = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("sponsor window call failed: {error}"));
                        continue;
                    }
                };
                let voting_start_offset = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("voting start offset call failed: {error}"));
                        continue;
                    }
                };
                let voting_yay_end_offset = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("yay end offset call failed: {error}"));
                        continue;
                    }
                };
                let voting_nay_end_offset = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("nay end offset call failed: {error}"));
                        continue;
                    }
                };
                let execution_start_offset = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("execution start offset call failed: {error}"));
                        continue;
                    }
                };
                let execution_end_offset = match values.next().expect("metadata length checked") {
                    Ok(value) => value,
                    Err(error) => {
                        query_rpc_pool.mark_bad_provider(&provider_handle);
                        last_error = Some(eyre!("execution end offset call failed: {error}"));
                        continue;
                    }
                };
                let rules = GovernanceContractRules {
                    sponsor_threshold,
                    quorum,
                    sponsor_window,
                    voting_start_offset,
                    voting_yay_end_offset,
                    voting_nay_end_offset,
                    execution_start_offset,
                    execution_end_offset,
                };
                if let Err(error) = rules.validate() {
                    query_rpc_pool.mark_bad_provider(&provider_handle);
                    last_error = Some(error);
                    continue;
                }
                return Ok(GovernanceContractSummary {
                    version,
                    address: voting_address,
                    proposal_count,
                    rules,
                });
            }
            Ok(Ok(_)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!(
                    "governance metadata multicall returned wrong result count"
                ));
            }
            Ok(Err(error)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!("governance metadata multicall failed: {error}"));
            }
            Err(_) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre!(
                    "governance metadata multicall timed out after {} milliseconds",
                    GOVERNANCE_RPC_TIMEOUT.as_millis()
                ));
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
    use serde_json::{Value, json};

    use super::*;

    const MULTICALL: Address = address!("0x0000000000000000000000000000000000000604");

    struct GovernanceFixture {
        url: String,
        calls: mpsc::Receiver<Vec<(Address, U256)>>,
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
            for _ in 0..3 {
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
}
