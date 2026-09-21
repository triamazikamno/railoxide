use std::ops::Range;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, eyre};
use railgun_wallet::keys::derive_executor_signer;
use zeroize::Zeroizing;

use super::ExecutorOwner;
use crate::hardware::HardwareDerivationDescriptor;
use crate::signer::EvmTransactionSigner as _;
use crate::vault::{DesktopViewSession, ExecutorOperationId, SoftwareRailgunSpendSigner};

/// The reviewed native action. Allocation selects Execute's index later.
#[derive(Clone, PartialEq, Eq)]
pub enum HardwareExecutorAction {
    Execute(ExecutorOperationId),
    Recover(ExecutorOperationId),
    RecoverPrepared {
        operation: ExecutorOperationId,
        recovery: ExecutorOperationId,
    },
    Retry {
        operation: ExecutorOperationId,
        transaction: B256,
    },
    Restore(Range<u32>),
    Register(ExecutorOperationId),
    GasPayment {
        account: String,
        operation: ExecutorOperationId,
    },
    Public {
        account: String,
        operation: ExecutorOperationId,
    },
}

/// Captured before device I/O and consumed only by that approval's completion.
pub struct HardwareExecutorAuthorizationRequest {
    owner: Arc<ExecutorOwner>,
    view: Arc<DesktopViewSession>,
    action: HardwareExecutorAction,
    gas_payer: Option<ExecutorGasPayer>,
}

/// Native operation memory only. No serialization, cloning, Debug, or seed access.
pub struct HardwareExecutorAuthorization {
    request: HardwareExecutorAuthorizationRequest,
    private: Option<SoftwareRailgunSpendSigner>,
    material: Mutex<ExecutorMaterial>,
}

struct ExecutorGasPayer {
    uuid: String,
    address: Address,
    password: Zeroizing<String>,
    seed_session: Option<Arc<crate::vault::ProtectedSoftwareSeedSession>>,
}

struct ExecutorMaterial {
    seed: Option<Zeroizing<[u8; 64]>>,
    selected: Option<(ExecutorOperationId, u32, PrivateKeySigner)>,
    recovery: Option<ExecutorOperationId>,
    consumed: bool,
}

impl ExecutorOwner {
    pub(super) fn recovery_authorization_action(
        &self,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        prepared: &super::PreparedExecutorRecovery,
    ) -> Result<HardwareExecutorAction> {
        self.ensure_active()?;
        if let crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.recovery_action(prepared)
        } else {
            Ok(HardwareExecutorAction::Recover(prepared.operation()))
        }
    }

    pub(super) fn require_executor_authorization(
        &self,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        action: &HardwareExecutorAction,
    ) -> Result<()> {
        self.require_executor_source(authorization, action)?;
        if !matches!(
            authorization,
            crate::DesktopPrivateSpendAuthorization::HardwareExecutor(_)
        ) {
            // Validate before waiting, without retaining an unlocked spend key across the wait.
            authorization.executor_spend_grant(&self.vault)?;
        }
        Ok(())
    }

    /// Check owner and custody/action binding without unlocking software credentials.
    pub(super) fn require_executor_source(
        &self,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        action: &HardwareExecutorAction,
    ) -> Result<()> {
        self.ensure_active()?;
        if let crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.require_action(self, action)
        } else {
            if self.view.hardware_profile_session().is_some() {
                return Err(eyre!(
                    "this wallet requires fresh hardware derivation for the reviewed action"
                ));
            }
            Ok(())
        }
    }

    pub(super) fn authorized_executor_signer(
        &self,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        action: &HardwareExecutorAction,
        operation: ExecutorOperationId,
        index: u32,
    ) -> Result<PrivateKeySigner> {
        self.require_executor_source(authorization, action)?;
        if let crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            return hardware
                .select(self, action, operation, index, None)
                .map(|(signer, _)| signer);
        }
        let (mut grant, seed) = authorization.executor_spend_grant(&self.vault)?;
        self.vault
            .executor_spend_signers_for_session(
                &mut grant,
                &self.view,
                seed,
                self.chain.chain_id,
                index,
            )
            .map(|(_, signer)| signer)
            .map_err(Into::into)
    }

    pub(super) fn authorized_executor_addresses(
        &self,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        operation: ExecutorOperationId,
        index: u32,
        spare: Option<u32>,
    ) -> Result<(Address, Option<Address>)> {
        let action = HardwareExecutorAction::Execute(operation);
        self.require_executor_source(authorization, &action)?;
        if let crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            return hardware
                .select(self, &action, operation, index, spare)
                .map(|(signer, spare)| (signer.address(), spare));
        }
        let (mut grant, seed) = authorization.executor_spend_grant(&self.vault)?;
        self.vault
            .executor_preparation_addresses_for_session(
                &mut grant,
                &self.view,
                seed,
                self.chain.chain_id,
                index,
                spare,
            )
            .map_err(Into::into)
    }

    pub fn hardware_authorization_request(
        self: &Arc<Self>,
        view: Arc<DesktopViewSession>,
        action: HardwareExecutorAction,
    ) -> Result<HardwareExecutorAuthorizationRequest> {
        self.ensure_active()?;
        if !self.view.is_same_wallet_session(&view) || view.hardware_profile_session().is_none() {
            return Err(eyre!("hardware approval belongs to another wallet session"));
        }
        if let HardwareExecutorAction::Restore(range) = &action
            && (range.is_empty()
                || range.end > (1 << 31)
                || range.end - range.start > crate::vault::MAX_EXECUTOR_DISCOVERY_RANGE)
        {
            return Err(eyre!("select a restoration range of at most 64 accounts"));
        }
        Ok(HardwareExecutorAuthorizationRequest {
            owner: Arc::clone(self),
            view,
            action,
            gas_payer: None,
        })
    }
}

impl HardwareExecutorAuthorizationRequest {
    /// Reject an invalidated owner before starting device I/O.
    pub fn ensure_active(&self) -> Result<()> {
        self.owner.ensure_active()
    }

    /// Authorize the reviewed software/imported payer before device I/O or allocation.
    pub fn with_gas_payer(
        mut self,
        uuid: String,
        password: Zeroizing<String>,
        seed_session: Option<Arc<crate::vault::ProtectedSoftwareSeedSession>>,
    ) -> Result<Self> {
        self.ensure_active()?;
        if !matches!(self.action, HardwareExecutorAction::Execute(_)) {
            return Err(eyre!("gas-payer authorization requires an executor action"));
        }
        let account = self
            .owner
            .vault
            .list_public_accounts_for_session(&self.view, true)?
            .into_iter()
            .find(|account| account.public_account_uuid == uuid)
            .ok_or_else(|| eyre!("reviewed gas payer is unavailable"))?;
        if !matches!(
            account.source,
            crate::vault::PublicAccountSource::Derived
                | crate::vault::PublicAccountSource::Imported
        ) {
            return Err(eyre!(
                "Select a software or imported gas payer, or a broadcaster."
            ));
        }
        let mut grant = self.owner.vault.create_spend_grant(&password)?;
        let key = self.owner.vault.public_account_signing_key_with_session(
            &mut grant,
            &self.view,
            &uuid,
            seed_session.as_deref(),
        )?;
        let signer = crate::signer::SoftwareEvmSigner::from_private_key(*key)?;
        if signer.address() != account.address {
            return Err(eyre!("reviewed gas payer identity changed"));
        }
        self.ensure_active()?;
        self.gas_payer = Some(ExecutorGasPayer {
            uuid,
            address: account.address,
            password,
            seed_session,
        });
        Ok(self)
    }

    pub fn complete(
        self,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
    ) -> Result<HardwareExecutorAuthorization> {
        self.owner.ensure_active()?;
        let (seed, private) = self
            .owner
            .vault
            .hardware_seed_and_signer_for_session(&self.view, descriptor, entropy)?;
        self.owner.ensure_active()?;
        let private = matches!(
            self.action,
            HardwareExecutorAction::Execute(_)
                | HardwareExecutorAction::Recover(_)
                | HardwareExecutorAction::RecoverPrepared { .. }
                | HardwareExecutorAction::GasPayment { .. }
        )
        .then_some(private);
        let (seed, selected) = match &self.action {
            HardwareExecutorAction::Execute(_) | HardwareExecutorAction::Restore(_) => {
                (Some(seed), None)
            }
            HardwareExecutorAction::Public { operation, .. }
            | HardwareExecutorAction::GasPayment { operation, .. }
            | HardwareExecutorAction::Register(operation)
            | HardwareExecutorAction::Retry { operation, .. }
            | HardwareExecutorAction::Recover(operation)
            | HardwareExecutorAction::RecoverPrepared { operation, .. } => {
                let record = self
                    .owner
                    .store
                    .records()?
                    .into_iter()
                    .find(|record| record.operation() == *operation)
                    .ok_or_else(|| eyre!("saved account is unavailable"))?;
                let signer = derive_executor_signer(
                    &seed,
                    self.view.derivation_index(),
                    self.owner.chain.chain_id,
                    record.index(),
                )?;
                drop(seed);
                (None, Some((*operation, record.index(), signer)))
            }
        };
        self.owner.ensure_active()?;
        Ok(HardwareExecutorAuthorization {
            request: self,
            private,
            material: Mutex::new(ExecutorMaterial {
                seed,
                selected,
                recovery: None,
                consumed: false,
            }),
        })
    }
}

impl HardwareExecutorAuthorization {
    #[cfg(test)]
    pub(crate) fn retains_root_seed_for_test(&self) -> bool {
        self.material.lock().unwrap().seed.is_some()
    }

    pub(in crate::desktop) const fn is_gas_payment(&self) -> bool {
        matches!(
            self.request.action,
            HardwareExecutorAction::GasPayment { .. }
        )
    }

    pub(in crate::desktop) fn take_private_signer(
        &mut self,
        view: &DesktopViewSession,
    ) -> Result<SoftwareRailgunSpendSigner> {
        self.private_signer(view)?;
        self.private
            .take()
            .ok_or_else(|| eyre!("Hardware private signing authority is unavailable"))
    }

    pub fn is_gas_payment_for(&self, account: &str) -> bool {
        matches!(&self.request.action, HardwareExecutorAction::GasPayment { account: approved, .. } if approved == account)
    }

    pub(super) fn bind_recovery(&self, recovery: ExecutorOperationId) -> Result<()> {
        let mut material = self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?;
        if material.recovery.is_some() || material.consumed {
            return Err(eyre!("Review and authorize a fresh recovery action."));
        }
        material.recovery = Some(recovery);
        Ok(())
    }

    pub(super) fn recovery_action(
        &self,
        prepared: &super::PreparedExecutorRecovery,
    ) -> Result<HardwareExecutorAction> {
        let action = HardwareExecutorAction::RecoverPrepared {
            operation: prepared.operation(),
            recovery: prepared.recovery(),
        };
        if self.request.action == action {
            return Ok(action);
        }
        let material = self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?;
        if material.recovery != Some(prepared.recovery()) {
            return Err(eyre!(
                "Hardware approval belongs to another recovery review."
            ));
        }
        Ok(HardwareExecutorAction::Recover(prepared.operation()))
    }

    pub(super) fn consume(&self) -> Result<()> {
        let mut material = self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?;
        if material.consumed {
            return Err(eyre!("Hardware approval has already been consumed."));
        }
        material.consumed = true;
        material.seed = None;
        material.selected = None;
        Ok(())
    }

    pub fn gas_payer_password(&self, uuid: &str) -> Result<Zeroizing<String>> {
        self.request.owner.ensure_active()?;
        let payer = self
            .request
            .gas_payer
            .as_ref()
            .filter(|payer| payer.uuid == uuid)
            .ok_or_else(|| eyre!("Authorize the reviewed gas payer again."))?;
        Ok(payer.password.clone())
    }

    pub(in crate::desktop) fn gas_payer_seed_session(
        &self,
    ) -> Option<Arc<crate::vault::ProtectedSoftwareSeedSession>> {
        self.request
            .gas_payer
            .as_ref()
            .and_then(|payer| payer.seed_session.clone())
    }

    pub(super) fn require_gas_payer(&self, sender: Address) -> Result<()> {
        let payer = self
            .request
            .gas_payer
            .as_ref()
            .filter(|payer| payer.address == sender)
            .ok_or_else(|| {
                eyre!("Authorize the reviewed software or imported gas payer before preparation.")
            })?;
        let current = self
            .request
            .owner
            .vault
            .list_public_accounts_for_session(&self.request.view, true)?;
        if !current.iter().any(|account| {
            account.public_account_uuid == payer.uuid
                && account.address == sender
                && matches!(
                    account.source,
                    crate::vault::PublicAccountSource::Derived
                        | crate::vault::PublicAccountSource::Imported
                )
        }) {
            return Err(eyre!("reviewed gas payer changed"));
        }
        Ok(())
    }

    pub(super) fn require_action(
        &self,
        owner: &ExecutorOwner,
        action: &HardwareExecutorAction,
    ) -> Result<()> {
        self.request.owner.ensure_active()?;
        if self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?
            .consumed
        {
            return Err(eyre!(
                "Approve a fresh hardware derivation for this action."
            ));
        }
        if !std::ptr::eq(owner, self.request.owner.as_ref()) || self.request.action != *action {
            return Err(eyre!(
                "hardware approval belongs to another action or wallet session"
            ));
        }
        Ok(())
    }

    pub(in crate::desktop) fn private_signer(
        &self,
        view: &DesktopViewSession,
    ) -> Result<&SoftwareRailgunSpendSigner> {
        self.require_action(&self.request.owner, &self.request.action)?;
        if !self.request.view.is_same_wallet_session(view) {
            return Err(eyre!("hardware approval belongs to another wallet session"));
        }
        self.private
            .as_ref()
            .ok_or_else(|| eyre!("this hardware approval does not authorize private spending"))
    }

    pub(in crate::desktop) fn into_private_signer(
        self,
        view: &DesktopViewSession,
    ) -> Result<SoftwareRailgunSpendSigner> {
        self.private_signer(view)?;
        self.private
            .ok_or_else(|| eyre!("hardware private signing authorization is unavailable"))
    }

    pub(super) fn select(
        &self,
        owner: &ExecutorOwner,
        action: &HardwareExecutorAction,
        operation: ExecutorOperationId,
        index: u32,
        spare: Option<u32>,
    ) -> Result<(PrivateKeySigner, Option<Address>)> {
        self.require_action(owner, action)?;
        let mut material = self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?;
        if let Some((selected_operation, selected_index, signer)) = &material.selected {
            if *selected_operation != operation || *selected_index != index || spare.is_some() {
                return Err(eyre!("hardware approval has already selected its executor"));
            }
            return Ok((signer.clone(), None));
        }
        let seed = material
            .seed
            .take()
            .ok_or_else(|| eyre!("hardware derivation has already been consumed"))?;
        let derive = |index| {
            derive_executor_signer(
                &seed,
                self.request.view.derivation_index(),
                owner.chain.chain_id,
                index,
            )
        };
        let signer = derive(index)?;
        let spare_address = spare
            .map(|index| derive(index).map(|signer| signer.address()))
            .transpose()?;
        // The root and spare key are dropped here, before network inspection.
        material.selected = Some((operation, index, signer.clone()));
        Ok((signer, spare_address))
    }

    pub(super) fn restore(
        &self,
        owner: &ExecutorOwner,
        range: Range<u32>,
    ) -> Result<Vec<(u32, Address)>> {
        self.require_action(owner, &HardwareExecutorAction::Restore(range.clone()))?;
        let seed = self
            .material
            .lock()
            .map_err(|_| eyre!("hardware authorization is unavailable"))?
            .seed
            .take()
            .ok_or_else(|| eyre!("hardware derivation has already been consumed"))?;
        range
            .map(|index| {
                derive_executor_signer(
                    &seed,
                    self.request.view.derivation_index(),
                    owner.chain.chain_id,
                    index,
                )
                .map(|signer| (index, signer.address()))
                .map_err(Into::into)
            })
            .collect()
    }
}
