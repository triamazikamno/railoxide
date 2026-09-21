use super::{
    Arc, DesktopViewSession, ExecutorOperationId, ExecutorOwner, ExecutorReconciliationReport,
    ExecutorRecord, Future, Range, Result, eyre,
};
use crate::DesktopPrivateSpendAuthorization;
use crate::desktop::executor_discovery::inspect_for_recovery_signing;
use crate::vault::{PublicAccountMetadata, PublicAccountScope, PublicAccountSource};

/// Owned by the Public signer through signed-data handoff. Every signing caller
/// uses the same owner exclusion, including message and gateway signing.
pub(crate) struct ExecutorPublicSigningGuard {
    owner: Arc<ExecutorOwner>,
    _activity: tokio::sync::OwnedMutexGuard<()>,
}

impl ExecutorPublicSigningGuard {
    pub(crate) fn ensure_active(&self) -> Result<()> {
        self.owner.ensure_active()
    }

    pub(crate) async fn while_active<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        self.owner.while_active(work).await
    }

    pub(crate) async fn reconcile_history(
        &self,
        owner: &Arc<ExecutorOwner>,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<ExecutorReconciliationReport> {
        if !Arc::ptr_eq(owner, &self.owner) {
            return Err(eyre!("executor signing owner changed"));
        }
        owner.reconcile_history_admitted(operation, range).await
    }

    pub(crate) fn ensure_chain(&self, chain_id: Option<u64>) -> Result<()> {
        self.ensure_active()?;
        if chain_id != Some(self.owner.chain.chain_id) {
            return Err(eyre!(
                "this Public account is available only on its source chain"
            ));
        }
        Ok(())
    }
}

impl ExecutorOwner {
    pub async fn register_public_account(
        &self,
        operation: ExecutorOperationId,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PublicAccountMetadata> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("saved account is unavailable"))?;
        let signer = self.authorized_executor_signer(
            authorization,
            &super::HardwareExecutorAction::Register(record.operation()),
            record.operation(),
            record.index(),
        )?;
        self.ensure_active()?;
        let account = self
            .store
            .register_public_account(operation, signer.address())?;
        if let DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.consume()?;
        }
        self.notify_change();
        Ok(account)
    }

    pub(crate) async fn admit_public_signer(
        self: &Arc<Self>,
        view: &DesktopViewSession,
        account: &PublicAccountMetadata,
        grant: &mut crate::vault::SpendGrant,
        seed: Option<&crate::vault::ProtectedSoftwareSeedSession>,
    ) -> Result<(crate::signer::SoftwareEvmSigner, ExecutorPublicSigningGuard)> {
        self.admit_public_signer_with(view, account, |source| {
            self.vault
                .executor_spend_signers_for_session(
                    grant,
                    &self.view,
                    seed,
                    source.chain_id(),
                    source.index(),
                )
                .map(|(_, signer)| signer)
                .map_err(Into::into)
        })
        .await
    }

    pub(crate) async fn admit_authorized_public_signer(
        self: &Arc<Self>,
        view: &DesktopViewSession,
        account: &PublicAccountMetadata,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<(crate::signer::SoftwareEvmSigner, ExecutorPublicSigningGuard)> {
        let admitted = self
            .admit_public_signer_with(view, account, |source| {
                self.authorized_executor_signer(
                    authorization,
                    &super::HardwareExecutorAction::Public {
                        account: account.public_account_uuid.clone(),
                        operation: source.operation(),
                    },
                    source.operation(),
                    source.index(),
                )
            })
            .await?;
        if let DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.consume()?;
        }
        Ok(admitted)
    }

    pub(crate) async fn admit_authorized_gas_signer(
        self: &Arc<Self>,
        view: &DesktopViewSession,
        account: &PublicAccountMetadata,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<(crate::signer::SoftwareEvmSigner, ExecutorPublicSigningGuard)> {
        let admitted = self
            .admit_public_signer_with(view, account, |source| {
                self.authorized_executor_signer(
                    authorization,
                    &super::HardwareExecutorAction::GasPayment {
                        account: account.public_account_uuid.clone(),
                        operation: source.operation(),
                    },
                    source.operation(),
                    source.index(),
                )
            })
            .await?;
        if let DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.consume()?;
        }
        Ok(admitted)
    }

    async fn admit_public_signer_with(
        self: &Arc<Self>,
        view: &DesktopViewSession,
        account: &PublicAccountMetadata,
        derive: impl FnOnce(
            crate::vault::ExecutorPublicAccountSource,
        ) -> Result<alloy::signers::local::PrivateKeySigner>,
    ) -> Result<(crate::signer::SoftwareEvmSigner, ExecutorPublicSigningGuard)> {
        self.ensure_active()?;
        let activity = self.activity.clone().lock_owned().await;
        self.ensure_active()?;
        // Admission may have waited behind another action; re-read activation and
        // source metadata after acquiring the owner, before deriving any key.
        let account = self
            .vault
            .list_public_accounts_for_session(view, true)?
            .into_iter()
            .find(|current| current.public_account_uuid == account.public_account_uuid)
            .ok_or_else(|| eyre!("Public account is unavailable"))?;
        let PublicAccountSource::ExecutorDerived(source) = account.source else {
            return Err(eyre!("Public account has no saved derivation reference"));
        };
        if !view.is_same_wallet_session(&self.view)
            || source.chain_id() != self.chain.chain_id
            || account.scope
                != (PublicAccountScope::PrivateWallet {
                    wallet_uuid: self.view.wallet_id().to_owned(),
                })
            || !account.is_active_for_wallet(self.view.wallet_id())
        {
            return Err(eyre!(
                "Public account belongs to another wallet or chain session"
            ));
        }
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == source.operation())
            .ok_or_else(|| eyre!("saved account is unavailable"))?;
        if record.public_account_uuid() != Some(account.public_account_uuid.as_str())
            || record.index() != source.index()
            || record.address() != Some(account.address)
        {
            return Err(eyre!("Public account no longer matches its saved identity"));
        }
        if !record.issued().is_empty() || !record.recovery_transactions().is_empty() {
            // Never trust a previous session's completion or an observation that
            // may have been reorganized away. No local payload means no scan.
            let mut chain = self.chain.clone();
            chain
                .railgun
                .as_mut()
                .ok_or_else(|| eyre!("chain does not support Railgun"))?
                .deployment
                .relay_adapt_7702_contract = record.delegate();
            chain.enabled = true;
            let (inspection, nonce) = self
                .while_active(inspect_for_recovery_signing(
                    &chain,
                    &self.http,
                    account.address,
                    &[],
                    !record.issued().is_empty(),
                    None,
                ))
                .await?;
            let current = self
                .reconcile_recovery_before_signing(&record, &chain, &inspection, nonce)
                .await?;
            require_resolved_public_work(&current)?;
        }
        let signer = derive(source)?;
        if signer.address() != account.address {
            return Err(eyre!("Public account signing identity changed"));
        }
        self.ensure_active()?;
        Ok((
            crate::signer::SoftwareEvmSigner::from_private_key(signer.to_bytes().0)?,
            ExecutorPublicSigningGuard {
                owner: self.clone(),
                _activity: activity,
            },
        ))
    }
}

fn require_resolved_public_work(record: &ExecutorRecord) -> Result<()> {
    if record.has_unresolved_issued_work() {
        return Err(eyre!(
            "An earlier signed operation is unresolved. Wait for it to finish, or use Recover in Stealth accounts before spending in Public."
        ));
    }
    Ok(())
}
