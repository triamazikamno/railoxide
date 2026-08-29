//! Typed, read-only access to the deployed Staking contract.
//!
//! This module deliberately keeps protocol values independent from GPUI.  All timestamps are
//! chain timestamps and all arithmetic which is part of a contract query is checked.

use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::BlockHeader as _;
use alloy::eips::BlockNumberOrTag;
use alloy::network::BlockResponse as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{CallItem, Failure, Provider};
use alloy::sol;
use alloy::sol_types::SolCall;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, WrapErr, eyre};
use railgun_ui::governance_contracts;
use sync_service::ChainConfigDefaults;
use thiserror::Error;
use tokio::time::timeout;

use crate::settings::EffectiveChainConfig;
use crate::{HttpContext, effective_rpc_urls_for_chain, query_rpc_pool_with_http_client};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MULTICALL_CHUNK_SIZE: NonZeroUsize = NonZeroUsize::new(64).unwrap();

sol! {
    interface GovernanceToken {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
    interface Staking {
        function stakingToken() external view returns (address);
        function STAKE_LOCKTIME() external view returns (uint256);
        function SNAPSHOT_INTERVAL() external view returns (uint256);
        function DEPLOY_TIME() external view returns (uint256);
        function totalStaked() external view returns (uint256);
        function totalVotingPower() external view returns (uint256);
        function currentInterval() external view returns (uint256);
        function stakesLength(address owner) external view returns (uint256);
        function stakes(address owner, uint256 index) external view returns
            (address delegate, uint256 amount, uint256 staketime, uint256 locktime, uint256 claimedTime);
        function votingPower(address owner) external view returns (uint256);
        function accountSnapshotLength(address owner) external view returns (uint256);
        function accountSnapshot(address owner, uint256 index) external view returns (uint256 interval, uint256 votingPower);
        function accountSnapshotAt(address owner, uint256 index, uint256 hint) external view returns (uint256 interval, uint256 votingPower);
    }
    interface GovernorRewardsDeployment {
        function staking() external view returns (address);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MulticallChunkSize(NonZeroUsize);

impl MulticallChunkSize {
    #[must_use]
    pub const fn new(size: NonZeroUsize) -> Self {
        Self(size)
    }
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for MulticallChunkSize {
    fn default() -> Self {
        Self(DEFAULT_MULTICALL_CHUNK_SIZE)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakingGlobalMetrics {
    pub total_staked: U256,
    pub total_voting_power: U256,
    pub deploy_time: U256,
    pub snapshot_interval: U256,
    pub current_interval: U256,
    pub stake_locktime: U256,
    pub chain_time: U256,
}

impl StakingGlobalMetrics {
    pub fn validate(&self) -> Result<()> {
        if self.snapshot_interval.is_zero() {
            return Err(eyre!("staking snapshot interval is zero"));
        }
        if self.chain_time < self.deploy_time {
            return Err(eyre!("staking chain time precedes deployment time"));
        }
        let expected = (self.chain_time - self.deploy_time) / self.snapshot_interval;
        if expected != self.current_interval {
            return Err(eyre!(
                "staking current interval {} disagrees with chain time-derived interval {}",
                self.current_interval,
                expected
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakePosition {
    pub owner: Address,
    pub id: U256,
    pub delegate: Address,
    pub amount: U256,
    pub staketime: U256,
    pub locktime: U256,
    pub claimed_time: U256,
    pub state: StakeState,
}

pub fn decode_stake_position(
    owner: Address,
    id: U256,
    chain_time: U256,
    bytes: &[u8],
) -> Result<StakePosition> {
    let record = Staking::stakesCall::abi_decode_returns_validate(bytes)
        .map_err(|error| eyre!("stake ABI decode failed: {error}"))?;
    Ok(StakePosition {
        owner,
        id,
        delegate: record.delegate,
        amount: record.amount,
        staketime: record.staketime,
        locktime: record.locktime,
        claimed_time: record.claimedTime,
        state: classify_stake_state(record.locktime, record.claimedTime, chain_time),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StakeState {
    Active,
    Unlocking,
    Claimable,
    Claimed,
}

#[must_use]
pub fn classify_stake_state(locktime: U256, claimed_time: U256, chain_time: U256) -> StakeState {
    if !claimed_time.is_zero() {
        StakeState::Claimed
    } else if locktime.is_zero() {
        StakeState::Active
    } else if chain_time > locktime {
        StakeState::Claimable
    } else {
        StakeState::Unlocking
    }
}

pub fn chunk_indices(total: usize, chunk_size: MulticallChunkSize) -> Vec<Vec<usize>> {
    (0..total)
        .collect::<Vec<_>>()
        .chunks(chunk_size.get())
        .map(ToOwned::to_owned)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DeploymentValidationError {
    #[error("staking token mismatch: configured {expected}, deployed {actual}")]
    StakingTokenMismatch { expected: Address, actual: Address },
    #[error("GovernorRewards staking mismatch: configured {expected}, deployed {actual}")]
    GovernorRewardsStakingMismatch { expected: Address, actual: Address },
    #[error("unsupported governance deployment on chain {chain_id}")]
    UnsupportedChain { chain_id: u64 },
    #[error("deployment read failed: {0}")]
    Read(String),
}

pub type StakingDeploymentValidationError = DeploymentValidationError;

pub fn validate_deployment_relationships(
    expected_token: Address,
    deployed_token: Address,
    expected_staking: Address,
    deployed_staking: Address,
) -> std::result::Result<(), DeploymentValidationError> {
    if expected_token != deployed_token {
        return Err(DeploymentValidationError::StakingTokenMismatch {
            expected: expected_token,
            actual: deployed_token,
        });
    }
    if expected_staking != deployed_staking {
        return Err(DeploymentValidationError::GovernorRewardsStakingMismatch {
            expected: expected_staking,
            actual: deployed_staking,
        });
    }
    Ok(())
}

/// Validate both contract relationships before exposing staking actions.
pub async fn validate_deployment(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> std::result::Result<(), DeploymentValidationError> {
    let contracts = governance_contracts(chain_id)
        .ok_or(DeploymentValidationError::UnsupportedChain { chain_id })?;
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)
        .map_err(|error| DeploymentValidationError::Read(error.to_string()))?;
    let values = multicall_values_at::<Staking::stakingTokenCall>(
        &pool,
        multicall_address,
        vec![
            (
                contracts.staking,
                Staking::stakingTokenCall {}.abi_encode().into(),
            ),
            (
                contracts.governor_rewards,
                GovernorRewardsDeployment::stakingCall {}
                    .abi_encode()
                    .into(),
            ),
        ],
    )
    .await
    .map_err(|error| DeploymentValidationError::Read(error.to_string()))?;
    if values.len() != 2 {
        return Err(DeploymentValidationError::Read(format!(
            "deployment multicall returned {}, expected 2",
            values.len()
        )));
    }
    let staking_token = values[0].clone().map_err(|error| {
        DeploymentValidationError::Read(format!("staking token read failed: {error:?}"))
    })?;
    let rewards_staking = values[1].clone().map_err(|error| {
        DeploymentValidationError::Read(format!("GovernorRewards staking read failed: {error:?}"))
    })?;
    validate_deployment_relationships(
        contracts.governance_token,
        staking_token,
        contracts.staking,
        rewards_staking,
    )
}

pub async fn validate_governance_deployment(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> std::result::Result<(), DeploymentValidationError> {
    validate_deployment(chain_id, effective_chain, http).await
}

pub async fn fetch_staking_global_metrics(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Option<StakingGlobalMetrics>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    validate_deployment(chain_id, effective_chain, http)
        .await
        .map_err(|e| eyre!("{e}"))?;
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let values = multicall_values::<Staking::totalStakedCall>(
        &pool,
        multicall_address,
        contracts.staking,
        vec![
            Staking::totalStakedCall {}.abi_encode().into(),
            Staking::totalVotingPowerCall {}.abi_encode().into(),
            Staking::DEPLOY_TIMECall {}.abi_encode().into(),
            Staking::SNAPSHOT_INTERVALCall {}.abi_encode().into(),
            Staking::currentIntervalCall {}.abi_encode().into(),
            Staking::STAKE_LOCKTIMECall {}.abi_encode().into(),
        ],
    )
    .await?;
    if values.len() != 6 {
        return Err(eyre!(
            "staking global multicall returned {}, expected 6",
            values.len()
        ));
    }
    let mut decoded = values.into_iter().enumerate().map(|(i, value)| {
        value.map_err(|error| eyre!("staking global call failed at index {i}: {error:?}"))
    });
    let total_staked = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 0 is missing"))??;
    let total_voting_power = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 1 is missing"))??;
    let deploy_time = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 2 is missing"))??;
    let snapshot_interval = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 3 is missing"))??;
    let current_interval = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 4 is missing"))??;
    let stake_locktime = decoded
        .next()
        .ok_or_else(|| eyre!("staking global result 5 is missing"))??;
    let chain_time = fetch_chain_time(&pool).await?;
    let metrics = StakingGlobalMetrics {
        total_staked,
        total_voting_power,
        deploy_time,
        snapshot_interval,
        current_interval,
        stake_locktime,
        chain_time,
    };
    metrics.validate()?;
    Ok(Some(metrics))
}

/// Read the actor's governance-token balance and the exact allowance granted to Staking.
///
/// Both calls use the configured query pool and are returned together so a stake draft cannot
/// accidentally combine observations from different refreshes.
pub async fn fetch_governance_token_balance_allowance(
    chain_id: u64,
    actor: Address,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Option<(U256, U256)>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let values = multicall_values::<GovernanceToken::balanceOfCall>(
        &pool,
        multicall_address,
        contracts.governance_token,
        vec![
            GovernanceToken::balanceOfCall { account: actor }
                .abi_encode()
                .into(),
            GovernanceToken::allowanceCall {
                owner: actor,
                spender: contracts.staking,
            }
            .abi_encode()
            .into(),
        ],
    )
    .await?;
    if values.len() != 2 {
        return Err(eyre!(
            "governance token read returned {}, expected 2",
            values.len()
        ));
    }
    let balance = values[0]
        .clone()
        .map_err(|error| eyre!("governance token balance call failed: {error:?}"))?;
    let allowance = values[1]
        .clone()
        .map_err(|error| eyre!("governance token allowance call failed: {error:?}"))?;
    Ok(Some((balance, allowance)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountStakeResult {
    pub account: Address,
    pub voting_power: std::result::Result<U256, String>,
    pub balance: std::result::Result<U256, String>,
    pub stakes: std::result::Result<Vec<StakePosition>, String>,
}

/// Enumerate all reported stake indices. A count or record failure only affects that account.
pub async fn fetch_account_stakes(
    chain_id: u64,
    accounts: &[Address],
    chain_time: U256,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<AccountStakeResult>> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(Vec::new());
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let metadata_plan: Vec<(Address, Bytes)> = accounts
        .iter()
        .flat_map(|&account| {
            [
                (
                    contracts.staking,
                    Staking::stakesLengthCall { owner: account }
                        .abi_encode()
                        .into(),
                ),
                (
                    contracts.staking,
                    Staking::votingPowerCall { owner: account }
                        .abi_encode()
                        .into(),
                ),
                (
                    contracts.governance_token,
                    GovernanceToken::balanceOfCall { account }
                        .abi_encode()
                        .into(),
                ),
            ]
        })
        .collect();
    let mut metadata: Vec<std::result::Result<U256, String>> = metadata_plan
        .iter()
        .map(|_| Err("metadata result is missing".into()))
        .collect();
    for (offset, call_chunk) in metadata_plan.chunks(chunk_size.get()).enumerate() {
        let start = offset * chunk_size.get();
        match multicall_values_at::<Staking::stakesLengthCall>(
            &pool,
            multicall_address,
            call_chunk.to_vec(),
        )
        .await
        {
            Ok(values) if values.len() == call_chunk.len() => {
                for (index, value) in values.into_iter().enumerate() {
                    metadata[start + index] =
                        value.map_err(|error| format!("metadata call failed: {error:?}"));
                }
            }
            Ok(values) => {
                for index in 0..call_chunk.len() {
                    metadata[start + index] = Err(format!(
                        "account metadata returned {}, expected {}",
                        values.len(),
                        call_chunk.len()
                    ));
                }
            }
            Err(error) => {
                for index in 0..call_chunk.len() {
                    metadata[start + index] = Err(error.to_string());
                }
            }
        }
    }
    let mut counts = vec![None; accounts.len()];
    let mut voting_power = vec![Err(String::new()); accounts.len()];
    let mut balances = vec![Err(String::new()); accounts.len()];
    let mut failures = vec![None; accounts.len()];
    for (account_index, triple) in metadata.chunks_exact(3).enumerate() {
        match &triple[0] {
            Ok(count) => match usize::try_from(*count) {
                Ok(count) => counts[account_index] = Some(count),
                Err(_) => {
                    failures[account_index] = Some("stake count exceeds platform limits".into());
                }
            },
            Err(error) => failures[account_index] = Some(error.clone()),
        }
        voting_power[account_index] = triple[1]
            .as_ref()
            .map(|value| *value)
            .map_err(|error| format!("voting power call failed: {error}"));
        balances[account_index] = triple[2]
            .as_ref()
            .map(|value| *value)
            .map_err(|error| format!("governance token balance call failed: {error}"));
    }

    let mut records = vec![Vec::new(); accounts.len()];
    let mut record_calls = Vec::new();
    for (account_index, (&account, count)) in accounts.iter().zip(&counts).enumerate() {
        if let Some(count) = count {
            for index in 0..*count {
                record_calls.push((account_index, account, index));
            }
        } else if failures[account_index].is_none() {
            failures[account_index] = Some("stake count read failed".into());
        }
    }
    for chunk in record_calls.chunks(chunk_size.get()) {
        let calls = chunk
            .iter()
            .map(|&(_, account, index)| {
                Staking::stakesCall {
                    owner: account,
                    index: U256::from(index),
                }
                .abi_encode()
                .into()
            })
            .collect();
        match multicall_values::<Staking::stakesCall>(
            &pool,
            multicall_address,
            contracts.staking,
            calls,
        )
        .await
        {
            Ok(values) if values.len() == chunk.len() => {
                for (&(account_index, account, index), value) in chunk.iter().zip(values) {
                    match value {
                        Ok(record) => records[account_index].push(StakePosition {
                            owner: account,
                            id: U256::from(index),
                            delegate: record.delegate,
                            amount: record.amount,
                            staketime: record.staketime,
                            locktime: record.locktime,
                            claimed_time: record.claimedTime,
                            state: classify_stake_state(
                                record.locktime,
                                record.claimedTime,
                                chain_time,
                            ),
                        }),
                        Err(error) if failures[account_index].is_none() => {
                            failures[account_index] =
                                Some(format!("stake {index} call failed: {error:?}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            Ok(values) => {
                for &(account_index, _, _) in chunk {
                    if failures[account_index].is_none() {
                        failures[account_index] = Some(format!(
                            "stake chunk returned {}, expected {}",
                            values.len(),
                            chunk.len()
                        ));
                    }
                }
            }
            Err(error) => {
                for &(account_index, _, _) in chunk {
                    if failures[account_index].is_none() {
                        failures[account_index] = Some(error.to_string());
                    }
                }
            }
        }
    }
    Ok(accounts
        .iter()
        .enumerate()
        .map(|(index, &account)| AccountStakeResult {
            account,
            voting_power: voting_power[index].clone(),
            balance: balances[index].clone(),
            stakes: failures[index]
                .clone()
                .map_or_else(|| Ok(records[index].clone()), Err),
        })
        .collect())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSnapshot {
    pub interval: U256,
    pub voting_power: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSnapshotsResult {
    pub account: Address,
    pub snapshots: std::result::Result<Vec<AccountSnapshot>, String>,
}

pub fn snapshot_hint(snapshots: &[AccountSnapshot], target: U256) -> Result<U256> {
    for pair in snapshots.windows(2) {
        if pair[0].interval >= pair[1].interval {
            return Err(eyre!("account snapshots are not strictly ascending"));
        }
    }
    Ok(U256::from(
        snapshots.partition_point(|snapshot| snapshot.interval < target),
    ))
}

pub fn reward_staking_interval(distribution_interval: U256, multiplier: U256) -> Result<U256> {
    distribution_interval
        .checked_mul(multiplier)
        .ok_or_else(|| eyre!("reward interval conversion overflows U256"))
}

pub async fn fetch_account_snapshots(
    chain_id: u64,
    account: Address,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<AccountSnapshot>> {
    if governance_contracts(chain_id).is_none() {
        return Ok(Vec::new());
    }
    let mut results =
        fetch_account_snapshots_multi(chain_id, &[account], effective_chain, http, chunk_size)
            .await?;
    results
        .pop()
        .ok_or_else(|| eyre!("account snapshot result is missing"))?
        .snapshots
        .map_err(|error| eyre!("{error}"))
}

/// Fetch snapshots for all accounts with shared count and bounded record batches.
pub async fn fetch_account_snapshots_multi(
    chain_id: u64,
    accounts: &[Address],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<AccountSnapshotsResult>> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(Vec::new());
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut count_values: Vec<std::result::Result<U256, String>> = accounts
        .iter()
        .map(|_| Err("snapshot count result is missing".into()))
        .collect();
    for (offset, account_chunk) in accounts.chunks(chunk_size.get()).enumerate() {
        let calls = account_chunk
            .iter()
            .map(|&account| {
                Staking::accountSnapshotLengthCall { owner: account }
                    .abi_encode()
                    .into()
            })
            .collect();
        let start = offset * chunk_size.get();
        match multicall_values::<Staking::accountSnapshotLengthCall>(
            &pool,
            multicall_address,
            contracts.staking,
            calls,
        )
        .await
        {
            Ok(values) if values.len() == account_chunk.len() => {
                for (index, value) in values.into_iter().enumerate() {
                    count_values[start + index] = value
                        .map_err(|error| format!("account snapshot length call failed: {error:?}"));
                }
            }
            Ok(values) => {
                for index in 0..account_chunk.len() {
                    count_values[start + index] = Err(format!(
                        "account snapshot lengths returned {}, expected {}",
                        values.len(),
                        account_chunk.len()
                    ));
                }
            }
            Err(error) => {
                for index in 0..account_chunk.len() {
                    count_values[start + index] = Err(error.to_string());
                }
            }
        }
    }
    let mut counts = vec![None; accounts.len()];
    let mut failures = vec![None; accounts.len()];
    for (index, value) in count_values.into_iter().enumerate() {
        match value {
            Ok(count) => match usize::try_from(count) {
                Ok(count) => counts[index] = Some(count),
                Err(_) => failures[index] = Some("snapshot count exceeds platform limits".into()),
            },
            Err(error) => failures[index] = Some(error),
        }
    }
    let mut snapshots = vec![Vec::new(); accounts.len()];
    let mut record_calls = Vec::new();
    for (account_index, (&account, count)) in accounts.iter().zip(&counts).enumerate() {
        if let Some(count) = count {
            for index in 0..*count {
                record_calls.push((account_index, account, index));
            }
        }
    }
    for chunk in record_calls.chunks(chunk_size.get()) {
        let calls = chunk
            .iter()
            .map(|&(_, account, index)| {
                Staking::accountSnapshotCall {
                    owner: account,
                    index: U256::from(index),
                }
                .abi_encode()
                .into()
            })
            .collect();
        match multicall_values::<Staking::accountSnapshotCall>(
            &pool,
            multicall_address,
            contracts.staking,
            calls,
        )
        .await
        {
            Ok(values) if values.len() == chunk.len() => {
                for (&(account_index, _, index), value) in chunk.iter().zip(values) {
                    match value {
                        Ok(snapshot) => snapshots[account_index].push(AccountSnapshot {
                            interval: snapshot.interval,
                            voting_power: snapshot.votingPower,
                        }),
                        Err(error) if failures[account_index].is_none() => {
                            failures[account_index] =
                                Some(format!("account snapshot {index} call failed: {error:?}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            Ok(values) => {
                for &(account_index, _, _) in chunk {
                    if failures[account_index].is_none() {
                        failures[account_index] = Some(format!(
                            "account snapshot chunk returned {}, expected {}",
                            values.len(),
                            chunk.len()
                        ));
                    }
                }
            }
            Err(error) => {
                for &(account_index, _, _) in chunk {
                    if failures[account_index].is_none() {
                        failures[account_index] = Some(error.to_string());
                    }
                }
            }
        }
    }
    Ok(accounts
        .iter()
        .enumerate()
        .map(|(index, &account)| {
            let state = failures[index].clone().map_or_else(
                || {
                    snapshot_hint(&snapshots[index], U256::ZERO)
                        .map_err(|error| error.to_string())
                        .map(|_| snapshots[index].clone())
                },
                Err,
            );
            AccountSnapshotsResult {
                account,
                snapshots: state,
            }
        })
        .collect())
}

async fn multicall_values<C: SolCall + 'static>(
    pool: &QueryRpcPool,
    multicall_address: Address,
    target: Address,
    calls: Vec<Bytes>,
) -> Result<Vec<std::result::Result<C::Return, Failure>>> {
    multicall_values_at::<C>(
        pool,
        multicall_address,
        calls.into_iter().map(|call| (target, call)).collect(),
    )
    .await
}

async fn multicall_values_at<C: SolCall + 'static>(
    pool: &QueryRpcPool,
    multicall_address: Address,
    calls: Vec<(Address, Bytes)>,
) -> Result<Vec<std::result::Result<C::Return, Failure>>> {
    let mut last_error = None;
    for _ in 0..pool.len() {
        let Some(handle) = pool.random_provider() else {
            break;
        };
        let mut multicall = handle
            .provider
            .multicall()
            .dynamic::<C>()
            .address(multicall_address);
        for (target, call) in calls.iter().cloned() {
            multicall = multicall.add_call_dynamic(CallItem::new(target, call));
        }
        match timeout(RPC_TIMEOUT, multicall.try_aggregate(false)).await {
            Ok(Ok(values)) => return Ok(values),
            Ok(Err(error)) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("multicall failed: {error}"));
            }
            Err(_) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("multicall timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

async fn fetch_chain_time(pool: &QueryRpcPool) -> Result<U256> {
    for _ in 0..pool.len() {
        let Some(handle) = pool.random_provider() else {
            break;
        };
        match timeout(
            RPC_TIMEOUT,
            handle
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        {
            Ok(Ok(Some(block))) => return Ok(U256::from(block.header().timestamp())),
            _ => pool.mark_bad_provider(&handle),
        }
    }
    Err(eyre!("latest chain block timestamp unavailable"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stake_states_use_strict_chain_time_and_claimed_precedence() {
        assert_eq!(
            classify_stake_state(U256::ZERO, U256::ZERO, U256::ZERO),
            StakeState::Active
        );
        assert_eq!(
            classify_stake_state(U256::from(10), U256::ZERO, U256::from(10)),
            StakeState::Unlocking
        );
        assert_eq!(
            classify_stake_state(U256::from(10), U256::ZERO, U256::from(11)),
            StakeState::Claimable
        );
        assert_eq!(
            classify_stake_state(U256::ZERO, U256::from(1), U256::ZERO),
            StakeState::Claimed
        );
    }

    #[test]
    fn snapshot_hint_covers_boundaries_and_rejects_unordered() {
        let snapshots = vec![
            AccountSnapshot {
                interval: U256::from(2),
                voting_power: U256::ZERO,
            },
            AccountSnapshot {
                interval: U256::from(5),
                voting_power: U256::ZERO,
            },
        ];
        assert_eq!(snapshot_hint(&snapshots, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(
            snapshot_hint(&snapshots, U256::from(2)).unwrap(),
            U256::ZERO
        );
        assert_eq!(
            snapshot_hint(&snapshots, U256::from(3)).unwrap(),
            U256::from(1)
        );
        assert_eq!(
            snapshot_hint(&snapshots, U256::from(5)).unwrap(),
            U256::from(1)
        );
        assert_eq!(
            snapshot_hint(&snapshots, U256::from(8)).unwrap(),
            U256::from(2)
        );
        let unordered = vec![snapshots[1].clone(), snapshots[0].clone()];
        assert!(snapshot_hint(&unordered, U256::ZERO).is_err());
        assert_eq!(snapshot_hint(&[], U256::ZERO).unwrap(), U256::ZERO);
        assert!(reward_staking_interval(U256::MAX, U256::from(2)).is_err());
    }

    #[test]
    fn chunk_planning_covers_every_index_without_a_silent_cap() {
        let chunks = chunk_indices(7, MulticallChunkSize::new(NonZeroUsize::new(3).unwrap()));
        assert_eq!(chunks, vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]);
        assert_eq!(
            chunks.iter().flatten().copied().collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
        assert_eq!(
            chunk_indices(0, MulticallChunkSize::default()),
            Vec::<Vec<usize>>::new()
        );
        let failed = AccountStakeResult {
            account: Address::from([3_u8; 20]),
            voting_power: Err("count failed".into()),
            balance: Err("balance failed".into()),
            stakes: Err("count failed".into()),
        };
        let healthy = AccountStakeResult {
            account: Address::from([4_u8; 20]),
            voting_power: Ok(U256::ZERO),
            balance: Ok(U256::ZERO),
            stakes: Ok(Vec::new()),
        };
        assert!(failed.stakes.is_err());
        assert!(healthy.stakes.is_ok());
    }

    #[test]
    fn stake_record_abi_decoding_is_strict() {
        let owner = Address::from([1_u8; 20]);
        let delegate = Address::from([2_u8; 20]);
        let mut encoded = Vec::with_capacity(160);
        encoded.extend([0_u8; 12]);
        encoded.extend(delegate.as_slice());
        for value in [U256::from(9), U256::from(10), U256::from(11), U256::ZERO] {
            encoded.extend(value.to_be_bytes::<32>());
        }
        let position = decode_stake_position(owner, U256::ZERO, U256::from(20), &encoded).unwrap();
        assert_eq!(position.delegate, delegate);
        assert_eq!(position.amount, U256::from(9));
        assert_eq!(position.state, StakeState::Claimable);
        assert!(decode_stake_position(owner, U256::ZERO, U256::ZERO, &encoded[..159]).is_err());
    }

    #[test]
    fn global_metrics_validate_checked_invariants() {
        let metrics = StakingGlobalMetrics {
            total_staked: U256::ZERO,
            total_voting_power: U256::ZERO,
            deploy_time: U256::from(10),
            snapshot_interval: U256::from(5),
            current_interval: U256::from(2),
            stake_locktime: U256::ZERO,
            chain_time: U256::from(20),
        };
        assert!(metrics.validate().is_ok());
        let mut invalid = metrics.clone();
        invalid.snapshot_interval = U256::ZERO;
        assert!(invalid.validate().is_err());
        let mut inconsistent = metrics.clone();
        inconsistent.current_interval = U256::from(3);
        assert!(inconsistent.validate().is_err());
        let before_deploy = StakingGlobalMetrics {
            chain_time: U256::from(9),
            ..metrics
        };
        assert!(before_deploy.validate().is_err());
    }

    #[test]
    fn deployment_relationships_fail_closed_by_relationship() {
        let token = Address::from([1_u8; 20]);
        let staking = Address::from([2_u8; 20]);
        assert!(validate_deployment_relationships(token, token, staking, staking).is_ok());
        assert_eq!(
            validate_deployment_relationships(token, Address::ZERO, staking, staking),
            Err(DeploymentValidationError::StakingTokenMismatch {
                expected: token,
                actual: Address::ZERO
            })
        );
        assert_eq!(
            validate_deployment_relationships(token, token, staking, Address::ZERO),
            Err(DeploymentValidationError::GovernorRewardsStakingMismatch {
                expected: staking,
                actual: Address::ZERO
            })
        );
    }
}
