//! Typed, read-only access to the deployed Staking contract.
//!
//! This module deliberately keeps protocol values independent from GPUI.  All timestamps are
//! chain timestamps and all arithmetic which is part of a contract query is checked.

use std::num::NonZeroUsize;

use alloy::consensus::BlockHeader as _;
use alloy::eips::BlockNumberOrTag;
use alloy::network::AnyRpcBlock;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};
use railgun_ui::governance_contracts;
use thiserror::Error;

use crate::{ClassRoute, HttpContext, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin};

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
    route: &ClassRoute,
    http: &HttpContext,
) -> std::result::Result<(), DeploymentValidationError> {
    let contracts = governance_contracts(chain_id)
        .ok_or(DeploymentValidationError::UnsupportedChain { chain_id })?;
    let broker = http.rpc_broker();
    let values = broker
        .submit_calls_decoded_as::<Staking::stakingTokenCall>(
            RpcRoute::from(route.clone()),
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
            WalletRpcOrigin::Staking.into(),
        )
        .await
        .map_err(|error| {
            DeploymentValidationError::Read(format!("RPC broker submission failed: {error}"))
        })?;
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
    route: &ClassRoute,
    http: &HttpContext,
) -> std::result::Result<(), DeploymentValidationError> {
    validate_deployment(chain_id, route, http).await
}

pub async fn fetch_staking_global_metrics(
    chain_id: u64,
    route: &ClassRoute,
    http: &HttpContext,
) -> Result<Option<StakingGlobalMetrics>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    validate_deployment(chain_id, route, http)
        .await
        .map_err(|e| eyre!("{e}"))?;
    let broker = http.rpc_broker();
    let values = broker
        .submit_calls_decoded_as::<Staking::totalStakedCall>(
            RpcRoute::from(route.clone()),
            vec![
                (
                    contracts.staking,
                    Staking::totalStakedCall {}.abi_encode().into(),
                ),
                (
                    contracts.staking,
                    Staking::totalVotingPowerCall {}.abi_encode().into(),
                ),
                (
                    contracts.staking,
                    Staking::DEPLOY_TIMECall {}.abi_encode().into(),
                ),
                (
                    contracts.staking,
                    Staking::SNAPSHOT_INTERVALCall {}.abi_encode().into(),
                ),
                (
                    contracts.staking,
                    Staking::currentIntervalCall {}.abi_encode().into(),
                ),
                (
                    contracts.staking,
                    Staking::STAKE_LOCKTIMECall {}.abi_encode().into(),
                ),
            ],
            WalletRpcOrigin::Staking.into(),
        )
        .await
        .map_err(|error| eyre!("RPC broker submission failed: {error}"))?;
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
    let chain_time = U256::from(
        fetch_latest_block(route, http, WalletRpcOrigin::Staking)
            .await?
            .header
            .timestamp(),
    );
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
    route: &ClassRoute,
    http: &HttpContext,
) -> Result<Option<(U256, U256)>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let broker = http.rpc_broker();
    let values = broker
        .submit_calls_decoded_as::<GovernanceToken::balanceOfCall>(
            RpcRoute::from(route.clone()),
            vec![
                (
                    contracts.governance_token,
                    GovernanceToken::balanceOfCall { account: actor }
                        .abi_encode()
                        .into(),
                ),
                (
                    contracts.governance_token,
                    GovernanceToken::allowanceCall {
                        owner: actor,
                        spender: contracts.staking,
                    }
                    .abi_encode()
                    .into(),
                ),
            ],
            WalletRpcOrigin::Staking.into(),
        )
        .await
        .map_err(|error| eyre!("RPC broker submission failed: {error}"))?;
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

/// Enumerate all of one account's reported stake indices. A count or record failure only affects
/// that account's stakes.
pub async fn fetch_account_stakes(
    chain_id: u64,
    account: Address,
    chain_time: U256,
    route: &ClassRoute,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<AccountStakeResult> {
    let contracts = governance_contracts(chain_id)
        .ok_or_else(|| eyre!("unsupported governance chain {chain_id}"))?;
    let broker = http.rpc_broker();
    let route = RpcRoute::from(route.clone());
    let metadata_plan: Vec<(Address, Bytes)> = vec![
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
    ];
    let mut metadata: Vec<std::result::Result<U256, String>> = Vec::with_capacity(3);
    for call_chunk in metadata_plan.chunks(chunk_size.get()) {
        match broker
            .submit_calls_decoded_as::<Staking::stakesLengthCall>(
                route.clone(),
                call_chunk.to_vec(),
                WalletRpcOrigin::Staking.into(),
            )
            .await
            .map_err(|error| eyre!("RPC broker submission failed: {error}"))
        {
            Ok(values) => metadata.extend(
                values
                    .into_iter()
                    .map(|value| value.map_err(|error| format!("metadata call failed: {error:?}"))),
            ),
            Err(error) => metadata.extend(call_chunk.iter().map(|_| Err(error.to_string()))),
        }
    }
    metadata.resize(3, Err("metadata result is missing".into()));
    let voting_power = metadata[1]
        .clone()
        .map_err(|error| format!("voting power call failed: {error}"));
    let balance = metadata[2]
        .clone()
        .map_err(|error| format!("governance token balance call failed: {error}"));
    let count = match &metadata[0] {
        Ok(count) => {
            usize::try_from(*count).map_err(|_| "stake count exceeds platform limits".to_string())
        }
        Err(error) => Err(error.clone()),
    };

    let mut records = Vec::new();
    let mut failure = count.as_ref().err().cloned();
    for chunk in chunk_indices(count.unwrap_or(0), chunk_size) {
        let calls: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&index| {
                (
                    contracts.staking,
                    Staking::stakesCall {
                        owner: account,
                        index: U256::from(index),
                    }
                    .abi_encode()
                    .into(),
                )
            })
            .collect();
        match broker
            .submit_calls_decoded_as::<Staking::stakesCall>(
                route.clone(),
                calls,
                WalletRpcOrigin::Staking.into(),
            )
            .await
            .map_err(|error| eyre!("RPC broker submission failed: {error}"))
        {
            Ok(values) => {
                for (&index, value) in chunk.iter().zip(values) {
                    match value {
                        Ok(record) => records.push(StakePosition {
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
                        Err(error) => {
                            failure.get_or_insert_with(|| {
                                format!("stake {index} call failed: {error:?}")
                            });
                        }
                    }
                }
            }
            Err(error) => {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
    }
    Ok(AccountStakeResult {
        account,
        voting_power,
        balance,
        stakes: failure.map_or(Ok(records), Err),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSnapshot {
    pub interval: U256,
    pub voting_power: U256,
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

/// Fetch one account's snapshots with a count read and bounded record batches.
pub async fn fetch_account_snapshots(
    chain_id: u64,
    account: Address,
    route: &ClassRoute,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<AccountSnapshot>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(Vec::new());
    };
    let broker = http.rpc_broker();
    let route = RpcRoute::from(route.clone());
    let count = broker
        .submit_calls_decoded_as::<Staking::accountSnapshotLengthCall>(
            route.clone(),
            vec![(
                contracts.staking,
                Staking::accountSnapshotLengthCall { owner: account }
                    .abi_encode()
                    .into(),
            )],
            WalletRpcOrigin::Staking.into(),
        )
        .await
        .map_err(|error| eyre!("RPC broker submission failed: {error}"))?
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("snapshot count result is missing"))?
        .map_err(|error| eyre!("account snapshot length call failed: {error:?}"))?;
    let count =
        usize::try_from(count).map_err(|_| eyre!("snapshot count exceeds platform limits"))?;
    let mut snapshots = Vec::new();
    for chunk in chunk_indices(count, chunk_size) {
        let calls: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&index| {
                (
                    contracts.staking,
                    Staking::accountSnapshotCall {
                        owner: account,
                        index: U256::from(index),
                    }
                    .abi_encode()
                    .into(),
                )
            })
            .collect();
        let values = broker
            .submit_calls_decoded_as::<Staking::accountSnapshotCall>(
                route.clone(),
                calls,
                WalletRpcOrigin::Staking.into(),
            )
            .await
            .map_err(|error| eyre!("RPC broker submission failed: {error}"))?;
        for (&index, value) in chunk.iter().zip(values) {
            let snapshot =
                value.map_err(|error| eyre!("account snapshot {index} call failed: {error:?}"))?;
            snapshots.push(AccountSnapshot {
                interval: snapshot.interval,
                voting_power: snapshot.votingPower,
            });
        }
    }
    snapshot_hint(&snapshots, U256::ZERO)?;
    Ok(snapshots)
}

/// Read the latest block over `route` through the broker, which keeps an isolated route's reads
/// on its own client. A query pool would not.
pub(crate) async fn fetch_latest_block(
    route: &ClassRoute,
    http: &HttpContext,
    origin: WalletRpcOrigin,
) -> Result<AnyRpcBlock> {
    let result = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            RpcRoute::from(route.clone()),
            vec![RpcRead::get_block_by_number(
                BlockNumberOrTag::Latest,
                false,
            )],
            origin.into(),
        ))
        .await
        .map_err(|error| eyre!("latest block RPC request failed: {error}"))?
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("latest block result is missing"))?
        .map_err(|error| eyre!("latest block RPC request failed: {error}"))?;
    serde_json::from_value(result.into_value())
        .map_err(|error| eyre!("latest block was unavailable: {error}"))
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
