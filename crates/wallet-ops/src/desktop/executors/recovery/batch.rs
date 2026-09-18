use alloy::primitives::U256;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::executor::multicall_signing_hash;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, eyre};

use super::{
    ExecutorOwner, ExecutorRecoveryExecution, ExecutorRecoveryFunding, ExecutorRecoveryStepOutcome,
    PreparedExecutorRecovery, recovery_funding_admission,
};
use crate::desktop::executor_discovery::inspect_for_recovery_batch;
use crate::desktop::executors::execution::authorize_delegation;
use crate::public_wallet::{VaultedPublicSigner, submit_executor_recovery_step};
use crate::settings::ExecutorProfile;
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    ExecutorPayloadContext, ExecutorPayloadPurpose, ExecutorPayloadStatus, IssuedExecutorPayload,
};
use crate::{
    DesktopPrivateSpendAuthorization, PublicActionProgressStep, PublicActionProgressUpdate,
};

impl ExecutorOwner {
    /// Execute the reviewed recovery atomically at the current contract nonce.
    /// A signed payload remains durable even if simulation, submission or local waiting fails.
    pub async fn submit_native_recovery_batch(
        &self,
        prepared: &PreparedExecutorRecovery,
        authorization: &DesktopPrivateSpendAuthorization,
        mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
    ) -> Result<ExecutorRecoveryStepOutcome> {
        self.ensure_active()?;
        let guard = self.lock_activity().await;
        let record = self.validate_recovery(prepared)?;
        let ExecutorRecoveryExecution::SignedMulticall { nonce } = prepared.execution else {
            return Err(eyre!(
                "this recovery was not reviewed as a signed multicall"
            ));
        };
        let ExecutorRecoveryFunding::ExecutorNative { gas_fee } = prepared.funding else {
            return Err(eyre!("signed multicall requires executor-native funding"));
        };
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("historical executor profile is unavailable"))?;
        let mut chain = self.chain.clone();
        chain.relay_adapt_7702_contract = record.delegate().to_string();
        chain.enabled = true;
        let (inspection, observed) = self
            .while_active(inspect_for_recovery_batch(
                &chain,
                &self.http,
                prepared.source,
                &[prepared.asset],
                prepared.replacement_nonce,
            ))
            .await?;
        prepared.validate_inspection(&inspection)?;
        if observed.nonce() != nonce {
            return Err(eyre!(
                "execution nonce changed; review the rebuilt recovery"
            ));
        }
        let record = self
            .reconcile_recovery_before_signing(&record, &chain, &inspection, Some(observed))
            .await?;
        recovery_funding_admission(
            &inspection,
            prepared.asset,
            prepared.amount,
            &prepared.funding,
            &prepared.gas_limits,
        )?;
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
        let hash = multicall_signing_hash(
            true,
            &prepared.calls,
            nonce,
            chain.chain_id,
            prepared.source,
        );
        let calldata = RelayAdapt7702::multicallCall {
            _requireSuccess: true,
            _calls: prepared.calls.clone(),
            _nonce: nonce,
            _signature: signer.sign_hash_sync(&hash)?.as_bytes().into(),
        }
        .abi_encode();
        let mut transaction = TransactionRequest::default()
            .from(prepared.source)
            .to(prepared.source)
            .value(U256::ZERO)
            .input(calldata.clone().into());
        transaction.chain_id = Some(chain.chain_id);
        transaction.gas = Some(prepared.gas_limits[0]);
        authorize_delegation(&mut transaction, profile, &inspection, &signer, true, false)?;
        let signer = VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key(
            signer.to_bytes().0,
        )?);
        self.ensure_active()?;
        self.store.record_issued(
            prepared.operation,
            IssuedExecutorPayload::new(
                nonce,
                profile.delegate(),
                hash,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(calldata.into(), observed, Vec::new()),
            ),
        )?;
        self.notify_change();
        let mut handoff = |transaction_hash, _: &TransactionRequest| {
            self.validate_recovery(prepared)?;
            self.store
                .record_submission(prepared.operation, hash, transaction_hash)?;
            self.notify_change();
            Ok(())
        };
        let receipt = self
            .while_active(Box::pin(submit_executor_recovery_step(
                PublicActionProgressStep::Shield,
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
        let observed = self
            .reconcile_history(
                prepared.operation,
                receipt.block_number..receipt.block_number + 1,
            )
            .await?;
        let status = observed
            .record()
            .payload_status(hash)
            .unwrap_or(ExecutorPayloadStatus::Uncertain);
        Ok(ExecutorRecoveryStepOutcome { receipt, status })
    }
}
