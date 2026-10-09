//! A swap executor's recovery and early cancellation.
//!
//! Both are executor recovery, one atomic batch at the current execution nonce. While an order
//! of the executor can still fill, the batch first invalidates it and resets the sell token's
//! settlement approval, so the order can't fill after the user gives up. Early cancellation is
//! that batch with no assets, paid by a broadcaster before the pre-hook runs. It consumes the
//! pre-hook's nonce `k`; if the pre-hook wins the race instead, the swap follows the
//! pre-hook-executed path and recovery with assets is offered.

use std::time::SystemTime;

use alloy::primitives::{Address, U256};
use broadcaster_core::contracts::executor::ExecutorAction;
use broadcaster_core::contracts::railgun::{Call, TokenData};
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;

use super::super::recovery::{PaidExecutionPurpose, recovery_allowance, recovery_gas_limits};
use super::super::{
    ExecutorOwner, ExecutorRecoveryFeeEstimate, ExecutorRecoveryFunding, PreparedExecutorRecovery,
};
use super::order::fillable_swap_orders;
use super::{SwapOrderState, swap_order_state};
use crate::settings::{EffectiveChainConfig, ExecutorProfile, SwapProfile};
use crate::vault::{ExecutorOperationId, ExecutorRecord};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorInspection, PublicActionProgressStep,
    PublicBroadcasterCandidate, WalletSession,
};

impl ExecutorOwner {
    /// Preview the private fee of an early cancellation without deriving keys or signing.
    /// Like any paid recovery, its fee comes from notes no executor operation reserves.
    pub async fn estimate_swap_cancellation_fee(
        &self,
        operation: ExecutorOperationId,
        session: &WalletSession,
        candidate: PublicBroadcasterCandidate,
    ) -> Result<ExecutorRecoveryFeeEstimate> {
        self.ensure_active()?;
        self.require_fee_session(session, PaidExecutionPurpose::Recovery)?;
        let record = self.swap_order_record(operation)?;
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("this swap executor's delegate is not supported"))?;
        let calls = swap_recovery_call_bound(&record);
        self.estimate_paid_execution_fee(
            PaidExecutionPurpose::Recovery,
            profile,
            candidate,
            &session.unspent_utxos(),
            |buffer| {
                recovery_gas_limits(
                    RailgunGasModel::for_chain(self.chain.chain_id),
                    &vec![PublicActionProgressStep::Approve; calls],
                    buffer,
                )[0]
            },
        )
        .await
    }

    /// Prepare an early cancellation of the swap's open order: a broadcaster-funded execute at
    /// the pre-hook's nonce `k` that carries only the private fee and `invalidateOrder`. It is
    /// recovery with no assets, reviewed and submitted through `submit_paid_recovery`.
    pub async fn prepare_swap_cancellation(
        &self,
        operation: ExecutorOperationId,
        candidate: PublicBroadcasterCandidate,
        maximum_private_fee: U256,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorRecovery> {
        self.swap_order_record(operation)?;
        self.prepare_recovery_amount(
            operation,
            None,
            U256::ZERO,
            ExecutorRecoveryFunding::PublicBroadcaster {
                candidate: Box::new(candidate),
                maximum_private_fee,
            },
            authorization,
            false,
        )
        .await
    }

    /// Calls a swap executor's recovery batch runs before its asset steps, read at the
    /// inspected block. Empty for other executors. A cancellation also requires the latest
    /// order to be open with its pre-hook at the current nonce.
    pub(in crate::desktop::executors) async fn swap_recovery_preparation(
        &self,
        chain: &EffectiveChainConfig,
        record: &ExecutorRecord,
        inspection: &ExecutorInspection,
        nonce: U256,
        cancellation: bool,
    ) -> Result<Vec<Call>> {
        let Some(swap) = record.swap() else {
            return Ok(Vec::new());
        };
        let profile = chain
            .swap_profile()
            .ok_or_else(|| eyre!("this swap executor's settlement contracts are unavailable"))?;
        let now = SystemTime::now();
        if cancellation {
            swap_cancellation_admitted(record, nonce, &profile, now)?;
        }
        let mut allowances = Vec::new();
        for order in swap.orders() {
            let token = swap.order_terms(order).sell_token();
            if allowances.iter().any(|(seen, _)| *seen == token) {
                continue;
            }
            let allowance = self
                .while_active(recovery_allowance(
                    chain,
                    &self.http,
                    inspection,
                    &TokenData::erc20(token),
                    profile.vault_relayer(),
                ))
                .await?;
            allowances.push((token, allowance));
        }
        swap_recovery_calls(record, &profile, inspection.address(), &allowances, now)
    }

    fn swap_order_record(&self, operation: ExecutorOperationId) -> Result<ExecutorRecord> {
        self.swap_record(operation)?
            .filter(|record| record.swap().is_some())
            .ok_or_else(|| eyre!("swap order is unavailable"))
    }
}

/// An upper bound on the calls `swap_recovery_calls` prepends, for fee reviews made before the
/// executor is inspected: at most one invalidation and one sell-token approval reset per order.
pub(in crate::desktop::executors) fn swap_recovery_call_bound(record: &ExecutorRecord) -> usize {
    record
        .swap()
        .map_or(0, |swap| swap.orders().len().saturating_mul(2))
}

/// `invalidateOrder` for every order of the executor that can still fill, then a reset of the
/// historical sell tokens' approvals to the vault relayer while allowances remain. An expired pre-hook's
/// order can't fill, so it adds nothing.
pub(crate) fn swap_recovery_calls(
    record: &ExecutorRecord,
    profile: &SwapProfile,
    executor: Address,
    sell_allowances: &[(Address, U256)],
    now: SystemTime,
) -> Result<Vec<Call>> {
    if record.swap().is_none() {
        return Ok(Vec::new());
    }
    let reset = sell_allowances
        .iter()
        .filter(|(_, allowance)| !allowance.is_zero())
        .map(|(token, _)| ExecutorAction::Approve {
            token: *token,
            spender: profile.vault_relayer(),
            amount: U256::ZERO,
        });
    fillable_swap_orders(record, profile, now)
        .into_iter()
        .map(|order_uid| ExecutorAction::InvalidateOrder {
            settlement: profile.settlement(),
            order_uid,
        })
        .chain(reset)
        .map(|action| Ok(action.call(executor)?))
        .collect()
}

/// Early cancellation takes the latest pre-hook's nonce before that pre-hook runs, so it
/// applies only while that order is open, can still fill, and its pre-hook holds `nonce`.
pub(crate) fn swap_cancellation_admitted(
    record: &ExecutorRecord,
    nonce: U256,
    profile: &SwapProfile,
    now: SystemTime,
) -> Result<()> {
    let order = record
        .swap()
        .and_then(|swap| swap.orders().last())
        .ok_or_else(|| eyre!("this swap has no order to cancel"))?;
    if swap_order_state(order) != SwapOrderState::Open || order.pre_hook().nonce() != nonce {
        return Err(eyre!(
            "this swap's pre-hook already ran or can no longer run; refresh the swap and recover the executor's assets if it holds any"
        ));
    }
    if !fillable_swap_orders(record, profile, now).contains(&order.uid()) {
        return Err(eyre!("this order can no longer fill; wait for its expiry"));
    }
    Ok(())
}
