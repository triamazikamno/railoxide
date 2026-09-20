use std::time::Instant;

use alloy::eips::eip7702::Authorization;
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, eyre};
use public_broadcaster_protocol::supports_nonce_bearing_executor;
use railgun_wallet::tx::ExecutorContext;
use railgun_wallet::{TransactionCall, Utxo};

use super::{
    ExecutorOwner, ExecutorRecoveryExecution, ExecutorRecoveryFunding, PreparedExecutorRecovery,
};
use crate::desktop::executor_discovery::{inspect_for_signing, matches_executor_delegation};
use crate::settings::ExecutorProfile;
use crate::vault::{
    ExecutorInputIdentity, ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose,
    IssuedExecutorPayload, ProtectedSoftwareSeedSession, SpendGrant,
};
use crate::{ExecutorActivity, ExecutorAsset, ExecutorInspection, PublicBroadcasterCandidate};

/// Funding selection precedes allocation. Existing sponsored Send and Unshield
/// select their legacy route before reaching this owner.
#[derive(Clone)]
pub enum ExecutorDelivery {
    PublicBroadcaster(Box<PublicBroadcasterCandidate>),
    SelfBroadcast { sender: Address, sponsored: bool },
    ManualExport,
}

impl ExecutorDelivery {
    pub fn admit(&self, profile: ExecutorProfile) -> Result<()> {
        match self {
            Self::PublicBroadcaster(candidate) => {
                if candidate.chain_id != profile.chain_id()
                    || candidate.available_wallets == 0
                    || candidate.fee_expiration <= std::time::SystemTime::now()
                    || !supports_nonce_bearing_executor(
                        candidate.relay_adapt_7702,
                        profile.delegate(),
                    )
                {
                    return Err(eyre!(
                        "selected broadcaster does not support this executor operation"
                    ));
                }
                candidate.parsed_required_poi_list_keys()?;
                Ok(())
            }
            Self::SelfBroadcast {
                sponsored: false, ..
            } => Ok(()),
            Self::SelfBroadcast {
                sponsored: true, ..
            } => Err(eyre!(
                "builder sponsorship is unavailable for executor operations"
            )),
            Self::ManualExport => Err(eyre!(
                "manual calldata export cannot represent executor delegation authorizations"
            )),
        }
    }

    const fn sender(&self) -> Option<Address> {
        match self {
            Self::SelfBroadcast { sender, .. } => Some(*sender),
            Self::PublicBroadcaster(_) | Self::ManualExport => None,
        }
    }
}

/// Native preparation identity carried through quotes and proof generation.
/// It contains no signing key and cannot be constructed from browser input.
pub struct PreparedExecutorOperation {
    operation: ExecutorOperationId,
    generation: u64,
    wallet_id: String,
    owner: tokio::sync::watch::Sender<bool>,
    context: ExecutorContext,
    delivery: ExecutorDelivery,
    inspection_assets: Vec<ExecutorAsset>,
    recovery: Option<std::sync::Arc<PreparedExecutorRecovery>>,
}

impl PreparedExecutorOperation {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn context(&self) -> ExecutorContext {
        self.context
    }

    #[must_use]
    pub const fn delivery(&self) -> &ExecutorDelivery {
        &self.delivery
    }

    pub(crate) fn require_broadcaster(&self, candidate: &PublicBroadcasterCandidate) -> Result<()> {
        let ExecutorDelivery::PublicBroadcaster(prepared) = &self.delivery else {
            return Err(eyre!("executor preparation selected another funding route"));
        };
        if prepared.chain_id != candidate.chain_id
            || prepared.railgun_address != candidate.railgun_address
            || prepared.fees_id != candidate.fees_id
            || prepared.fee != candidate.fee
            || prepared.token != candidate.token
            || prepared.fee_expiration != candidate.fee_expiration
            || prepared.viewing_public_key != candidate.viewing_public_key
            || prepared.required_poi_list_keys != candidate.required_poi_list_keys
        {
            return Err(eyre!(
                "executor broadcaster selection changed; refresh preparation"
            ));
        }
        Ok(())
    }
}

/// Returning this value is signed-data handoff: its payload is already durable.
pub struct IssuedExecutorTransaction {
    operation: ExecutorOperationId,
    payload_hash: B256,
    transaction: TransactionRequest,
}

fn require_unfinished_operation(record: &crate::vault::ExecutorRecord) -> Result<()> {
    if record.issued().iter().any(|payload| {
        record.payload_status(payload.hash()) == Some(crate::vault::ExecutorPayloadStatus::Executed)
    }) {
        return Err(eyre!(
            "this executor operation has already completed; start a new action"
        ));
    }
    Ok(())
}

impl IssuedExecutorTransaction {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn payload_hash(&self) -> B256 {
        self.payload_hash
    }

    #[must_use]
    pub const fn transaction(&self) -> &TransactionRequest {
        &self.transaction
    }
}

impl ExecutorOwner {
    /// Prepare with the same software authorization used by the private spend.
    pub async fn prepare_authorized_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
    ) -> Result<PreparedExecutorOperation> {
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "vault_unlock", "started");
        let result = authorization.executor_spend_grant(&self.vault);
        tracing::info!(
            target: "executor_preparation",
            step = "vault_unlock",
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "finished"
        );
        let (mut grant, seed) = result?;
        self.prepare_operation(
            operation,
            delivery,
            &mut grant,
            seed,
            assets,
            purpose_summary,
        )
        .await
    }

    /// Recheck native ownership before quoting or proving an existing preparation.
    pub fn validate_preparation(
        &self,
        prepared: &PreparedExecutorOperation,
    ) -> Result<crate::vault::ExecutorRecord> {
        self.ensure_active()?;
        if let Some(recovery) = &prepared.recovery {
            let record = self.validate_recovery(recovery)?;
            if !self.closed.same_channel(&prepared.owner)
                || prepared.generation != self.generation
                || prepared.wallet_id != self.view.wallet_id()
                || prepared.operation != record.operation()
                || prepared.context.executor != recovery.source()
                || prepared.context.chain_id != self.chain.chain_id
                || prepared.context.delegate != record.delegate()
                || recovery.execution()
                    != (ExecutorRecoveryExecution::PaidExecute {
                        nonce: prepared.context.execution_nonce,
                    })
            {
                return Err(eyre!(
                    "recovery execution preparation no longer matches its owner"
                ));
            }
            return Ok(record);
        }
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        if !self.closed.same_channel(&prepared.owner)
            || prepared.generation != self.generation
            || prepared.wallet_id != self.view.wallet_id()
            || prepared.context.chain_id != profile.chain_id()
            || prepared.context.delegate != profile.delegate()
        {
            return Err(eyre!(
                "executor preparation belongs to a different wallet session"
            ));
        }
        prepared.delivery.admit(profile)?;
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == prepared.operation)
            .ok_or_else(|| eyre!("executor reservation is unavailable"))?;
        if record.is_retired()
            || record.address() != Some(prepared.context.executor)
            || record.delegate() != prepared.context.delegate
        {
            return Err(eyre!(
                "executor preparation no longer matches its reservation"
            ));
        }
        require_unfinished_operation(&record)?;
        Ok(record)
    }

    /// Bind paid recovery to the retained identity without allocating or exposing
    /// another account. Proof and delivery code carry this same preparation.
    pub fn prepare_broadcaster_recovery_execution(
        &self,
        recovery: std::sync::Arc<PreparedExecutorRecovery>,
    ) -> Result<PreparedExecutorOperation> {
        let record = self.validate_recovery(&recovery)?;
        let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = recovery.funding()
        else {
            return Err(eyre!("this recovery selected another funding route"));
        };
        let ExecutorRecoveryExecution::PaidExecute { nonce } = recovery.execution() else {
            return Err(eyre!(
                "broadcaster recovery requires the current execute wrapper"
            ));
        };
        Ok(PreparedExecutorOperation {
            operation: record.operation(),
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: self.chain.chain_id,
                executor: recovery.source(),
                delegate: record.delegate(),
                execution_nonce: nonce,
            },
            delivery: ExecutorDelivery::PublicBroadcaster(candidate.clone()),
            inspection_assets: Vec::new(),
            recovery: Some(recovery),
        })
    }

    /// Reserve durably before deriving or querying the address. Failed and used
    /// reservations remain visible for recovery and never enter the free pool.
    pub async fn prepare_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        grant: &mut SpendGrant,
        protected_seed: Option<&ProtectedSoftwareSeedSession>,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
    ) -> Result<PreparedExecutorOperation> {
        self.ensure_active()?;
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "activity_lock", "started");
        let guard = self.lock_activity().await;
        tracing::info!(
            target: "executor_preparation",
            step = "activity_lock",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        self.ensure_active()?;
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        delivery.admit(profile)?;
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "reserve_and_derive", "started");
        let record = self
            .store
            .reserve(operation, profile.delegate(), purpose_summary, assets)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        let spare = self
            .store
            .reserve_spare(profile.delegate())
            .inspect_err(|_| {
                tracing::debug!(
                    "executor spare allocation deferred until the next authorized action"
                );
            })
            .ok();
        let spare_index = spare
            .as_ref()
            .and_then(|spare| spare.address().is_none().then_some(spare.index()));
        let (address, spare_address) = self.vault.executor_preparation_addresses_for_session(
            grant,
            &self.view,
            protected_seed,
            self.chain.chain_id,
            record.index(),
            spare_index,
        )?;
        self.ensure_active()?;
        self.store.bind_address(operation, address)?;
        if let Some((index, address)) = spare_index.zip(spare_address) {
            self.store.bind_spare_address(index, address)?;
        }
        self.notify_change();
        tracing::info!(
            target: "executor_preparation",
            step = "reserve_and_derive",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        drop(guard);
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "chain_inspection", "started");
        let result = if record.issued().is_empty() && record.recovery_transactions().is_empty() {
            self.unused_inspection(record.index(), address, assets)
                .await
        } else {
            self.while_active(inspect_for_signing(
                &self.chain,
                &self.http,
                address,
                assets,
            ))
            .await
            .map(|(inspection, observed)| {
                std::sync::Arc::new(super::spare::CheckedExecutor {
                    inspection,
                    observed,
                    revision: None,
                })
            })
        };
        tracing::info!(
            target: "executor_preparation",
            step = "chain_inspection",
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "finished"
        );
        let checked = result?;
        let inspection = &checked.inspection;
        let observed = checked.observed;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let record = self
            .store
            .reserve(operation, profile.delegate(), purpose_summary, assets)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        if record.issued().is_empty()
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || observed.nonce() != U256::ZERO)
        {
            self.store.retire(operation)?;
            return Err(eyre!(
                "reserved executor has prior activity; it remains available for recovery"
            ));
        }
        Ok(PreparedExecutorOperation {
            operation,
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: profile.chain_id(),
                executor: address,
                delegate: profile.delegate(),
                execution_nonce: observed.nonce(),
            },
            delivery,
            inspection_assets: assets.to_vec(),
            recovery: None,
        })
    }

    /// Sign the final proved call under current spend authorization. The shared
    /// context checks proof binding and hashes all transactions and action data.
    pub async fn issue_operation(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        grant: &mut SpendGrant,
        protected_seed: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<IssuedExecutorTransaction> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self.validate_preparation(prepared)?;
        let mut chain = Box::new(self.chain.clone());
        if prepared.recovery.is_some() {
            chain
                .railgun
                .as_mut()
                .ok_or_else(|| eyre!("chain does not support Railgun"))?
                .deployment
                .relay_adapt_7702_contract = record.delegate();
            chain.enabled = true;
        }
        let profile = chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let hash = prepared.context.signing_hash(call)?;
        // The durable reservation must cover precisely the private inputs whose
        // nullifiers the signed execute can spend, including every inner transaction.
        let decoded = RelayAdapt7702::executeCall::abi_decode(&call.data)?;
        if let Some(recovery) = &prepared.recovery
            && (decoded._actionData.minGasLimit != U256::ZERO
                || decoded._actionData.calls.len() != recovery.calls().len()
                || decoded._actionData.calls.iter().zip(recovery.calls()).any(
                    |(actual, expected)| {
                        actual.to != expected.to
                            || actual.value != expected.value
                            || actual.data != expected.data
                    },
                ))
        {
            return Err(eyre!(
                "proved recovery calls differ from the reviewed actions"
            ));
        }
        let mut expected = decoded
            ._transactions
            .iter()
            .flat_map(|transaction| {
                transaction
                    .nullifiers
                    .iter()
                    .map(|nullifier| (u32::from(transaction.boundParams.treeNumber), *nullifier))
            })
            .collect::<Vec<_>>();
        let nullifying_key = self.view.scan_keys().nullifying_key;
        let mut supplied = inputs
            .iter()
            .map(|input| (input.tree, B256::from(input.nullifier(nullifying_key))))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        supplied.sort_unstable();
        if expected != supplied || supplied.is_empty() {
            return Err(eyre!(
                "executor input reservations do not match the proved transaction"
            ));
        }
        if self.records()?.iter().any(|other| {
            other.operation() != prepared.operation
                && other
                    .reserved_inputs()
                    .iter()
                    .any(|reserved| inputs.iter().any(|input| reserved.matches(input)))
        }) {
            return Err(eyre!(
                "private inputs are reserved by another executor operation"
            ));
        }
        let checked = if prepared.recovery.is_none()
            && record.issued().is_empty()
            && record.recovery_transactions().is_empty()
        {
            self.unused_inspection(
                record.index(),
                prepared.context.executor,
                &prepared.inspection_assets,
            )
            .await?
        } else {
            let (inspection, observed) = if let Some(recovery) = &prepared.recovery {
                self.while_active(
                    crate::desktop::executor_discovery::inspect_for_recovery_batch(
                        &chain,
                        &self.http,
                        prepared.context.executor,
                        &[recovery.asset()],
                        recovery.replacement_nonce(),
                    ),
                )
                .await?
            } else {
                self.while_active(inspect_for_signing(
                    &chain,
                    &self.http,
                    prepared.context.executor,
                    &[],
                ))
                .await?
            };
            std::sync::Arc::new(super::spare::CheckedExecutor {
                inspection,
                observed,
                revision: None,
            })
        };
        let inspection = &checked.inspection;
        let observed = checked.observed;
        checked.ensure_valid()?;
        if observed.nonce() != prepared.context.execution_nonce {
            return Err(eyre!(
                "executor nonce changed; rebuild the approved operation"
            ));
        }
        if prepared.recovery.is_none()
            && record.issued().is_empty()
            && record.recovery_transactions().is_empty()
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || !observed.nonce().is_zero())
        {
            self.store.retire(prepared.operation)?;
            return Err(eyre!(
                "reserved executor has prior activity; it remains available for recovery"
            ));
        }
        if let Some(recovery) = &prepared.recovery {
            super::recovery::recovery_funding_admission(
                inspection,
                recovery.asset(),
                recovery.amount(),
                recovery.funding(),
                recovery.gas_limits(),
            )?;
            recovery.validate_inspection(inspection)?;
        }
        // Recheck known inclusions even when the latest page does not cover all
        // issued history. Unknown older winners keep future-nonce signing blocked.
        self.reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .remove(&prepared.operation);
        self.store.invalidate_observation(prepared.operation)?;
        let history = self
            .while_active(
                crate::desktop::executor_observation::observe_executor_history(
                    &chain,
                    &self.http,
                    &record,
                    observed.block().number..observed.block().number + 1,
                    Some(observed),
                ),
            )
            .await?;
        if history.nonce != Some(observed) {
            return Err(eyre!(
                "executor chain observation changed; retry preparation"
            ));
        }
        let reconciled = self
            .store
            .reconcile(prepared.operation, observed, &history.inclusions)?;
        if prepared.recovery.is_none() {
            require_unfinished_operation(&reconciled)?;
        }
        let (_private_signer, signer) = self.vault.executor_spend_signers_for_session(
            grant,
            &self.view,
            protected_seed,
            self.chain.chain_id,
            record.index(),
        )?;
        if signer.address() != prepared.context.executor {
            return Err(eyre!(
                "executor signing identity does not match preparation"
            ));
        }
        self.ensure_active()?;
        let signed_call = prepared
            .context
            .authorize_call(call, signer.sign_hash_sync(&hash)?)?;
        let mut transaction = TransactionRequest::default()
            .to(signed_call.to)
            .input(signed_call.data.clone().into())
            .value(U256::ZERO);
        transaction.chain_id = Some(profile.chain_id());
        transaction.from = prepared.delivery.sender();
        authorize_delegation(
            &mut transaction,
            profile,
            inspection,
            &signer,
            prepared.recovery.is_some(),
            matches!(prepared.delivery, ExecutorDelivery::PublicBroadcaster(_)),
        )?;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let mut payload_context = ExecutorPayloadContext::new(
            signed_call.data,
            observed,
            inputs
                .iter()
                .map(ExecutorInputIdentity::from_utxo)
                .collect(),
        );
        if checked.revision.is_some() {
            let history_start = self
                .unused
                .lock()
                .map_err(|_| eyre!("executor preparation is unavailable"))?
                .history_start(observed, self.chain.finality_depth);
            payload_context = payload_context.with_history_start(history_start);
        }
        self.store.record_issued(
            prepared.operation,
            IssuedExecutorPayload::new(
                observed.nonce(),
                profile.delegate(),
                hash,
                if prepared.recovery.is_some() {
                    ExecutorPayloadPurpose::Recovery
                } else {
                    ExecutorPayloadPurpose::Operation
                },
                payload_context,
            ),
        )?;
        self.unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?
            .remove(record.index());
        self.ensure_active()?;
        self.notify_change();
        Ok(IssuedExecutorTransaction {
            operation: prepared.operation,
            payload_hash: hash,
            transaction,
        })
    }
}

/// Execution and recovery share the Ethereum authorization nonce rules. The
/// contract execution nonce is signed separately into their respective calls.
pub(super) fn authorize_delegation(
    transaction: &mut TransactionRequest,
    profile: ExecutorProfile,
    inspection: &ExecutorInspection,
    signer: &PrivateKeySigner,
    recovery: bool,
    require_authorization: bool,
) -> Result<()> {
    let code = inspection
        .code()
        .ok_or_else(|| eyre!("executor delegation is unavailable"))?;
    let delegated = matches_executor_delegation(code, profile);
    // SDK broadcaster TX7702 requests require an authorization even when this
    // delegate is already installed. Native delivery can reuse it without one.
    if code.is_empty()
        || recovery
            && crate::desktop::executor_discovery::executor_delegation(code).is_some()
            && !delegated
        || require_authorization && delegated
    {
        let account_nonce = inspection
            .account_nonce()
            .ok_or_else(|| eyre!("executor account nonce is unavailable"))?;
        let nonce = if transaction.from == Some(signer.address()) {
            account_nonce
                .checked_add(1)
                .ok_or_else(|| eyre!("executor account nonce is exhausted"))?
        } else {
            account_nonce
        };
        let authorization = Authorization {
            chain_id: U256::from(profile.chain_id()),
            address: profile.delegate(),
            nonce,
        };
        let signature = signer.sign_hash_sync(&authorization.signature_hash())?;
        transaction.authorization_list = Some(vec![authorization.into_signed(signature)]);
        transaction.transaction_type = Some(4);
    } else if !delegated {
        return Err(eyre!("executor has an unrecognized delegation"));
    }
    if transaction.from == Some(signer.address()) {
        transaction.nonce = inspection.account_nonce();
    }
    Ok(())
}
