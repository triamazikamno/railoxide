use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, eyre};

use super::{
    ExecutorOwner, ExecutorRecoveryExecution, ExecutorRecoveryFunding, ExecutorRecoveryStepOutcome,
    recovery_execution,
};
use crate::desktop::executor_discovery::inspect_for_recovery_signing;
use crate::public_wallet::{VaultedPublicSigner, submit_executor_recovery_step};
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    ExecutorOperationId, ExecutorPayloadStatus, ExecutorRecord, ExecutorRecoveryStepKind,
    IssuedExecutorRecoveryTransaction,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, PublicActionGasFeeSelection,
    PublicActionProgressStep, PublicActionProgressUpdate,
};

/// A fresh native review may reprice a retained attempt, never change its call.
/// Completing this step may still leave other recovery steps to inspect and review.
pub struct PreparedExecutorRecoveryRetry {
    operation: ExecutorOperationId,
    original: IssuedExecutorRecoveryTransaction,
    source: Address,
    gas_fee: PublicActionGasFeeSelection,
    gas_limit: u64,
    remaining_gas_limit: u64,
    native_reserve: U256,
    generation: u64,
    owner: tokio::sync::watch::Sender<bool>,
}

impl PreparedExecutorRecoveryRetry {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn original(&self) -> &IssuedExecutorRecoveryTransaction {
        &self.original
    }
    #[must_use]
    pub const fn source(&self) -> Address {
        self.source
    }
    #[must_use]
    pub const fn gas_fee(&self) -> PublicActionGasFeeSelection {
        self.gas_fee
    }
    #[must_use]
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
    #[must_use]
    pub const fn native_reserve(&self) -> U256 {
        self.native_reserve
    }
}

impl ExecutorOwner {
    /// Local preparation only. The caller reviews the retained call, selected private
    /// wallet, and refreshed fees before providing spend authorization for the retry.
    pub fn prepare_ordinary_recovery_retry(
        &self,
        operation: ExecutorOperationId,
        hash: B256,
        gas_fee: PublicActionGasFeeSelection,
        gas_limit: u64,
    ) -> Result<PreparedExecutorRecoveryRetry> {
        self.ensure_active()?;
        let record = self.recovery_record(operation)?;
        let original = record
            .recovery_transactions()
            .iter()
            .find(|transaction| transaction.hash() == hash)
            .ok_or_else(|| eyre!("retained recovery transaction is unavailable"))?
            .clone();
        let source = record
            .address()
            .ok_or_else(|| eyre!("executor address is unavailable"))?;
        let PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } = gas_fee
        else {
            return Err(eyre!("review fixed gas fees before retrying recovery"));
        };
        if gas_limit == 0 || max_fee_per_gas == 0 || max_priority_fee_per_gas > max_fee_per_gas {
            return Err(eyre!("recovery retry gas limits or fees are invalid"));
        }
        let previous_gas = original
            .transaction()
            .gas
            .ok_or_else(|| eyre!("retained gas limit is unavailable"))?;
        let remaining_gas_limit = original
            .remaining_gas_limit()
            .checked_sub(previous_gas)
            .and_then(|remaining| remaining.checked_add(gas_limit))
            .ok_or_else(|| {
                eyre!("retained recovery gas reserve is unavailable or exceeds the supported limit")
            })?;
        Ok(PreparedExecutorRecoveryRetry {
            operation,
            original,
            source,
            gas_fee,
            gas_limit,
            remaining_gas_limit,
            native_reserve: U256::from(remaining_gas_limit) * U256::from(max_fee_per_gas),
            generation: self.generation,
            owner: self.closed.clone(),
        })
    }

    fn validate_recovery_retry(
        &self,
        prepared: &PreparedExecutorRecoveryRetry,
    ) -> Result<ExecutorRecord> {
        self.ensure_active()?;
        if !self.closed.same_channel(&prepared.owner) || self.generation != prepared.generation {
            return Err(eyre!(
                "recovery retry belongs to an inactive wallet session"
            ));
        }
        let record = self.recovery_record(prepared.operation)?;
        if record.address() != Some(prepared.source)
            || !record.recovery_transactions().iter().any(|transaction| {
                transaction.hash() == prepared.original.hash()
                    && transaction.transaction() == prepared.original.transaction()
                    && transaction.remaining_gas_limit() == prepared.original.remaining_gas_limit()
            })
        {
            return Err(eyre!("retained recovery identity changed"));
        }
        Ok(record)
    }

    pub async fn retry_ordinary_recovery_transaction(
        &self,
        prepared: &PreparedExecutorRecoveryRetry,
        authorization: &DesktopPrivateSpendAuthorization,
        mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
    ) -> Result<ExecutorRecoveryStepOutcome> {
        self.ensure_active()?;
        let guard = self.lock_activity().await;
        let record = self.validate_recovery_retry(prepared)?;
        let account_nonce = prepared
            .original
            .transaction()
            .nonce
            .ok_or_else(|| eyre!("retained account nonce is unavailable"))?;
        let mut chain = self.chain.clone();
        chain.relay_adapt_7702_contract = record.delegate().to_string();
        chain.enabled = true;
        let (inspection, nonce) = self
            .while_active(inspect_for_recovery_signing(
                &chain,
                &self.http,
                prepared.source,
                &[],
                !record.issued().is_empty(),
                Some(account_nonce),
            ))
            .await?;
        if inspection.account_nonce() != Some(account_nonce) {
            return Err(eyre!(
                "this transaction nonce has changed; reconcile and review the remaining recovery"
            ));
        }
        if recovery_execution(
            &record,
            &inspection,
            &ExecutorRecoveryFunding::ExecutorNative {
                gas_fee: prepared.gas_fee,
            },
        )? != ExecutorRecoveryExecution::Ordinary
        {
            return Err(eyre!(
                "an outstanding execution nonce requires a newly reviewed recovery batch"
            ));
        }
        let record = self
            .reconcile_recovery_before_signing(&record, &chain, &inspection, nonce)
            .await?;
        if record.recovery_transactions().iter().any(|transaction| {
            transaction.transaction().nonce == Some(account_nonce)
                && transaction.inclusion().is_some()
        }) {
            return Err(eyre!(
                "this transaction already has a canonical receipt; review the remaining recovery"
            ));
        }
        let required = prepared
            .native_reserve
            .checked_add(prepared.original.transaction().value.unwrap_or_default())
            .ok_or_else(|| eyre!("recovery native funding exceeds the supported limit"))?;
        if inspection
            .balances()
            .get(&ExecutorAsset::Native)
            .copied()
            .flatten()
            .is_none_or(|balance| balance < required)
        {
            return Err(eyre!(
                "insufficient native balance for the retry and remaining recovery gas; fund this executor before retrying"
            ));
        }
        let (mut grant, seed) = authorization.executor_spend_grant(&self.vault)?;
        let (_, signer) = self.vault.executor_spend_signers_for_session(
            &mut grant,
            &self.view,
            seed,
            self.chain.chain_id,
            record.index(),
        )?;
        if signer.address() != prepared.source {
            return Err(eyre!("recovery signer changed"));
        }
        let signer = VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key(
            signer.to_bytes().0,
        )?);
        let mut transaction = prepared.original.transaction().clone();
        transaction.gas = Some(prepared.gas_limit);
        let step = match prepared.original.kind() {
            ExecutorRecoveryStepKind::Wrap => PublicActionProgressStep::Wrap,
            ExecutorRecoveryStepKind::ApproveErc20 | ExecutorRecoveryStepKind::ApproveErc721 => {
                PublicActionProgressStep::Approve
            }
            ExecutorRecoveryStepKind::Shield => PublicActionProgressStep::Shield,
        };
        let observed = inspection.block();
        let mut handoff = |hash, transaction: &TransactionRequest| {
            self.validate_recovery_retry(prepared)?;
            self.store.record_recovery_transaction(
                prepared.operation,
                IssuedExecutorRecoveryTransaction::new(
                    prepared.original.recovery(),
                    prepared.original.step(),
                    prepared.original.kind(),
                    transaction.clone(),
                    hash,
                    observed,
                )
                .with_remaining_gas_limit(prepared.remaining_gas_limit),
            )?;
            self.notify_change();
            Ok(())
        };
        let receipt = self
            .while_active(Box::pin(submit_executor_recovery_step(
                step,
                transaction,
                &signer,
                &chain,
                prepared.gas_fee,
                &self.http,
                &mut handoff,
                &mut progress,
            )))
            .await?;
        drop(guard);
        let observed = self
            .reconcile_history(
                prepared.operation,
                receipt.block_number..receipt.block_number + 1,
            )
            .await?;
        let status = observed
            .record()
            .recovery_transaction_status(receipt.tx_hash.parse()?)
            .unwrap_or(ExecutorPayloadStatus::Uncertain);
        Ok(ExecutorRecoveryStepOutcome { receipt, status })
    }
}
