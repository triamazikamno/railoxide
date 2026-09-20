use super::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorInspection, ExecutorOwner,
    ExecutorRecord, ExecutorRecoveryExecution, ExecutorRecoveryFunding,
    ExecutorRecoveryStepOutcome, PreparedExecutorRecovery, PublicActionGasFeeSelection,
    PublicActionProgressStep, Result, TransactionRequest, U256, eyre, recovery_execution,
};
use crate::PublicActionProgressUpdate;
use crate::desktop::executor_discovery::inspect_for_recovery_signing;
use crate::public_wallet::{VaultedPublicSigner, submit_executor_recovery_step};
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    ExecutorPayloadStatus, ExecutorRecoveryStepKind, IssuedExecutorRecoveryTransaction,
};

impl ExecutorOwner {
    /// Submit one reviewed ordinary step. Each successful outer receipt is reconciled
    /// before the next step; it does not alone establish approval or shielding effects.
    pub async fn submit_ordinary_recovery_step(
        &self,
        prepared: &PreparedExecutorRecovery,
        step: usize,
        authorization: &DesktopPrivateSpendAuthorization,
        mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
    ) -> Result<ExecutorRecoveryStepOutcome> {
        self.ensure_active()?;
        let guard = self.lock_activity().await;
        let record = self.validate_recovery(prepared)?;
        if prepared.execution != ExecutorRecoveryExecution::Ordinary {
            return Err(eyre!("this reviewed recovery requires its signed batch"));
        }
        let ExecutorRecoveryFunding::ExecutorNative { gas_fee } = prepared.funding else {
            return Err(eyre!(
                "ordinary recovery requires the executor's native gas balance"
            ));
        };
        let call = prepared
            .calls
            .get(step)
            .ok_or_else(|| eyre!("recovery step is unavailable"))?;
        let mut chain = self.chain.clone();
        chain
            .railgun
            .as_mut()
            .ok_or_else(|| eyre!("chain does not support Railgun"))?
            .deployment
            .relay_adapt_7702_contract = record.delegate();
        chain.enabled = true;
        let mut assets = vec![prepared.asset];
        if prepared.asset == ExecutorAsset::Native {
            assets.push(ExecutorAsset::Erc20(
                prepared.shield.preimage.token.tokenAddress,
            ));
        }
        let (inspection, nonce) = self
            .while_active(inspect_for_recovery_signing(
                &chain,
                &self.http,
                prepared.source,
                &assets,
                !record.issued().is_empty(),
                None,
            ))
            .await?;
        if recovery_execution(&record, &inspection, &prepared.funding)? != prepared.execution {
            return Err(eyre!(
                "execution nonce changed; review the rebuilt recovery"
            ));
        }
        let record = self
            .reconcile_recovery_before_signing(&record, &chain, &inspection, nonce)
            .await?;
        require_recovery_step(
            &record,
            prepared,
            step,
            inspection
                .account_nonce()
                .ok_or_else(|| eyre!("executor account nonce is unavailable"))?,
        )?;
        require_remaining_funding(prepared, step, &inspection)?;
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
        let mut transaction = TransactionRequest::default()
            .to(call.to)
            .value(call.value)
            .input(call.data.clone().into());
        transaction.chain_id = Some(chain.chain_id);
        transaction.from = Some(prepared.source);
        transaction.nonce = inspection.account_nonce();
        transaction.gas = Some(prepared.gas_limits[step]);
        let kind = match prepared.steps[step] {
            PublicActionProgressStep::Wrap => ExecutorRecoveryStepKind::Wrap,
            PublicActionProgressStep::Shield => ExecutorRecoveryStepKind::Shield,
            PublicActionProgressStep::Approve => {
                if matches!(prepared.asset, ExecutorAsset::Erc721 { .. }) {
                    ExecutorRecoveryStepKind::ApproveErc721
                } else {
                    ExecutorRecoveryStepKind::ApproveErc20
                }
            }
            _ => return Err(eyre!("unsupported recovery step")),
        };
        let observed = inspection.block();
        let mut handoff = |hash, transaction: &TransactionRequest| {
            self.validate_recovery(prepared)?;
            self.store.record_recovery_transaction(
                prepared.operation,
                IssuedExecutorRecoveryTransaction::new(
                    prepared.recovery,
                    u32::try_from(step)?,
                    kind,
                    transaction.clone(),
                    hash,
                    observed,
                )
                .with_remaining_gas_limit(prepared.gas_limits[step..].iter().sum()),
            )?;
            self.notify_change();
            Ok(())
        };
        let receipt = self
            .while_active(Box::pin(submit_executor_recovery_step(
                prepared.steps[step],
                transaction,
                &signer,
                &chain,
                gas_fee,
                &self.http,
                &mut handoff,
                &mut progress,
            )))
            .await?;
        drop(guard);
        // Preserve the handed-off record even if this read fails or the owner closes.
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
    pub(in crate::desktop::executors) async fn reconcile_recovery_before_signing(
        &self,
        record: &ExecutorRecord,
        chain: &crate::settings::EffectiveChainConfig,
        inspection: &ExecutorInspection,
        nonce: Option<crate::vault::ExecutorNonceObservation>,
    ) -> Result<ExecutorRecord> {
        let operation = record.operation();
        self.reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .remove(&operation);
        self.store.invalidate_observation(operation)?;
        let number = nonce.map_or_else(
            || {
                inspection
                    .block()
                    .number
                    .saturating_sub(chain.finality_depth)
            },
            |nonce| nonce.block().number,
        );
        let history = self
            .while_active(
                crate::desktop::executor_observation::observe_executor_history(
                    chain,
                    &self.http,
                    record,
                    number..number + 1,
                    nonce,
                ),
            )
            .await?;
        if history.nonce != nonce {
            return Err(eyre!("executor state changed; refresh recovery"));
        }
        if let Some(nonce) = history.nonce {
            self.store
                .reconcile(operation, nonce, &history.inclusions)?;
        }
        let record = self.store.reconcile_recovery(
            operation,
            history.block,
            &history.recovery_inclusions,
        )?;
        self.reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .insert(operation);
        self.notify_change();
        Ok(record)
    }
}

fn require_recovery_step(
    record: &ExecutorRecord,
    prepared: &PreparedExecutorRecovery,
    step: usize,
    account_nonce: u64,
) -> Result<()> {
    for previous_step in 0..step {
        if !record.recovery_transactions().iter().any(|transaction| {
            transaction.recovery() == prepared.recovery
                && transaction.step() as usize == previous_step
                && record.recovery_transaction_status(transaction.hash())
                    == Some(ExecutorPayloadStatus::Executed)
        }) {
            return Err(eyre!(
                "the previous recovery step has not confirmed its expected effects"
            ));
        }
    }
    for transaction in record.recovery_transactions() {
        if transaction.recovery() == prepared.recovery && transaction.step() as usize == step {
            if transaction.inclusion().is_some()
                || transaction.transaction().nonce != Some(account_nonce)
            {
                return Err(eyre!(
                    "this recovery step already has a receipt or its nonce changed; refresh recovery"
                ));
            }
        } else if transaction
            .transaction()
            .nonce
            .is_some_and(|nonce| nonce >= account_nonce)
        {
            return Err(eyre!(
                "another recovery transaction remains outstanding; reconcile it before continuing"
            ));
        }
    }
    Ok(())
}

fn require_remaining_funding(
    prepared: &PreparedExecutorRecovery,
    step: usize,
    inspection: &ExecutorInspection,
) -> Result<()> {
    let ExecutorRecoveryFunding::ExecutorNative {
        gas_fee: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas, ..
        },
    } = prepared.funding
    else {
        return Err(eyre!("recovery requires reviewed native gas fees"));
    };
    let gas = prepared.gas_limits[step..]
        .iter()
        .fold(U256::ZERO, |sum, limit| sum + U256::from(*limit));
    let values = prepared.calls[step..]
        .iter()
        .fold(U256::ZERO, |sum, call| sum + call.value);
    let balance = inspection
        .balances()
        .get(&ExecutorAsset::Native)
        .copied()
        .flatten()
        .ok_or_else(|| eyre!("executor native balance is unknown"))?;
    if balance < gas * U256::from(max_fee_per_gas) + values {
        return Err(eyre!(
            "insufficient native balance for the remaining recovery gas; fund this executor or review another funding route"
        ));
    }
    let source_asset = if prepared.asset == ExecutorAsset::Native && values.is_zero() {
        ExecutorAsset::Erc20(prepared.shield.preimage.token.tokenAddress)
    } else {
        prepared.asset
    };
    if inspection
        .balances()
        .get(&source_asset)
        .copied()
        .flatten()
        .is_none_or(|balance| balance < prepared.amount)
    {
        return Err(eyre!(
            "recovery asset balance changed or is unknown; refresh recovery"
        ));
    }
    Ok(())
}
