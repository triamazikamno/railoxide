use std::sync::Arc;

use gpui::{Context, Window};
use wallet_ops::vault::{
    CreateSoftwareContextResult, SoftwareContextChainInput, SoftwareContextSyncIntent, SpendUnlock,
    VaultSessionId, ViewUnlock, WalletMetadataBundle, WalletSoftwareContextKind,
};
use zeroize::Zeroizing;

use super::super::{DesktopViewSession, VaultState, WalletRoot};
use super::passphrase_ui::{creation_chain_baseline, validate_pending_context_label};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PendingSoftwareProfileOpenStage {
    Choosing,
    UnknownDecision,
    CreationHandoff,
}

pub(in crate::root) struct PendingSoftwareProfileOpen {
    pub(in crate::root) base_profile_uuid: Arc<str>,
    pub(in crate::root) base_metadata: WalletMetadataBundle,
    pub(in crate::root) internal_metadata: Vec<WalletMetadataBundle>,
    pub(in crate::root) vault_view_unlock: Arc<ViewUnlock>,
    pub(in crate::root) spend_unlock: SpendUnlock,
    pub(in crate::root) vault_session_id: VaultSessionId,
    pub(in crate::root) operation_generation: u64,
    stage: PendingSoftwareProfileOpenStageState,
}

enum PendingSoftwareProfileOpenStageState {
    Choosing,
    UnknownDecision {
        first_passphrase: Zeroizing<String>,
    },
    CreationHandoff {
        first_passphrase: Option<Zeroizing<String>>,
    },
}

impl PendingSoftwareProfileOpen {
    pub(in crate::root) fn new(
        base_metadata: WalletMetadataBundle,
        internal_metadata: Vec<WalletMetadataBundle>,
        vault_view_unlock: Arc<ViewUnlock>,
        spend_unlock: SpendUnlock,
        vault_session_id: VaultSessionId,
        operation_generation: u64,
    ) -> Option<Self> {
        let context = base_metadata.software_context.as_ref()?;
        if context.kind != WalletSoftwareContextKind::Standard
            || context.base_profile_uuid != base_metadata.wallet_uuid
        {
            return None;
        }
        Some(Self {
            base_profile_uuid: Arc::from(base_metadata.wallet_uuid.clone()),
            base_metadata,
            internal_metadata,
            vault_view_unlock,
            spend_unlock,
            vault_session_id,
            operation_generation,
            stage: PendingSoftwareProfileOpenStageState::Choosing,
        })
    }

    pub(in crate::root) const fn stage(&self) -> PendingSoftwareProfileOpenStage {
        match &self.stage {
            PendingSoftwareProfileOpenStageState::Choosing => {
                PendingSoftwareProfileOpenStage::Choosing
            }
            PendingSoftwareProfileOpenStageState::UnknownDecision { .. } => {
                PendingSoftwareProfileOpenStage::UnknownDecision
            }
            PendingSoftwareProfileOpenStageState::CreationHandoff { .. } => {
                PendingSoftwareProfileOpenStage::CreationHandoff
            }
        }
    }

    pub(in crate::root) const fn set_operation_generation(&mut self, generation: u64) {
        self.operation_generation = generation;
    }

    pub(in crate::root) fn into_unknown_decision(
        mut self,
        first_passphrase: Zeroizing<String>,
    ) -> Self {
        self.stage = PendingSoftwareProfileOpenStageState::UnknownDecision { first_passphrase };
        self
    }

    pub(in crate::root) fn take_for_creation_passphrase(&mut self) -> Option<Zeroizing<String>> {
        let stage = std::mem::replace(
            &mut self.stage,
            PendingSoftwareProfileOpenStageState::CreationHandoff {
                first_passphrase: None,
            },
        );
        match stage {
            PendingSoftwareProfileOpenStageState::UnknownDecision { first_passphrase } => {
                Some(first_passphrase)
            }
            PendingSoftwareProfileOpenStageState::CreationHandoff {
                mut first_passphrase,
            } => first_passphrase.take(),
            other @ PendingSoftwareProfileOpenStageState::Choosing => {
                self.stage = other;
                None
            }
        }
    }

    pub(in crate::root) const fn is_choosing(&self) -> bool {
        matches!(&self.stage, PendingSoftwareProfileOpenStageState::Choosing)
    }

    pub(in crate::root) fn enter_creation_handoff(&mut self) -> bool {
        let stage = std::mem::replace(
            &mut self.stage,
            PendingSoftwareProfileOpenStageState::CreationHandoff {
                first_passphrase: None,
            },
        );
        match stage {
            PendingSoftwareProfileOpenStageState::UnknownDecision { first_passphrase } => {
                self.stage = PendingSoftwareProfileOpenStageState::CreationHandoff {
                    first_passphrase: Some(first_passphrase),
                };
                true
            }
            other @ (PendingSoftwareProfileOpenStageState::Choosing
            | PendingSoftwareProfileOpenStageState::CreationHandoff { .. }) => {
                self.stage = other;
                false
            }
        }
    }

    pub(in crate::root) fn passphrase_matches_confirmation(&self, confirmation: &str) -> bool {
        match &self.stage {
            PendingSoftwareProfileOpenStageState::UnknownDecision { first_passphrase }
            | PendingSoftwareProfileOpenStageState::CreationHandoff {
                first_passphrase: Some(first_passphrase),
            } => first_passphrase.as_bytes() == confirmation.as_bytes(),
            PendingSoftwareProfileOpenStageState::Choosing
            | PendingSoftwareProfileOpenStageState::CreationHandoff {
                first_passphrase: None,
            } => false,
        }
    }

    pub(in crate::root) fn retry(&mut self) {
        self.stage = PendingSoftwareProfileOpenStageState::Choosing;
    }
}

enum PendingPassphraseTaskResult {
    Known {
        session: DesktopViewSession,
        metadata: Vec<WalletMetadataBundle>,
        protected_seed_session: wallet_ops::vault::ProtectedSoftwareSeedSession,
    },
    Unknown(PendingSoftwareProfileOpen),
}

impl WalletRoot {
    pub(in crate::root) fn continue_pending_without_passphrase(
        &mut self,
        remember_standard_context: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut pending) = self.pending_software_profile_open.take() else {
            return;
        };
        if !pending.is_choosing() {
            self.pending_software_profile_open = Some(pending);
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.abandon_pending_software_profile_open(window, cx);
            return;
        };
        let operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = operation_generation;
        pending.set_operation_generation(operation_generation);
        let gateway_unlock = self.gateway_unlock_continuation();
        let active_wallet_generation = self.active_wallet_generation;
        let selected_chain = self.selected_chain;
        let base_profile_uuid = pending.base_profile_uuid.clone();
        let task_base_profile_uuid = base_profile_uuid.clone();
        let join = self.runtime.spawn_blocking(move || {
            if remember_standard_context {
                store.set_standard_context_auto_open_preference_with_view_unlock(
                    pending.vault_view_unlock.as_ref(),
                    task_base_profile_uuid.as_ref(),
                    true,
                )?;
            }
            let session = store.load_view_session_with_view_unlock(
                pending.vault_view_unlock.as_ref(),
                task_base_profile_uuid.as_ref(),
            )?;
            let metadata = store
                .list_wallet_metadata_with_view_unlock(pending.vault_view_unlock.as_ref(), true)?;
            Ok::<_, wallet_ops::vault::VaultError>((session, metadata))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.pending_software_profile_open_is_current(
                    operation_generation,
                    active_wallet_generation,
                    selected_chain,
                    base_profile_uuid.as_ref(),
                ) {
                    return;
                }
                root.pending_software_profile_open = None;
                match result {
                    Ok(Ok((session, metadata))) => {
                        root.install_verified_software_context(
                            session,
                            &metadata,
                            None,
                            gateway_unlock,
                            window,
                            cx,
                        );
                    }
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "standard software profile open task failed");
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn submit_pending_software_passphrase(
        &mut self,
        passphrase: Zeroizing<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut pending) = self.pending_software_profile_open.take() else {
            return;
        };
        if !pending.is_choosing() {
            self.pending_software_profile_open = Some(pending);
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.abandon_pending_software_profile_open(window, cx);
            return;
        };
        let operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = operation_generation;
        pending.set_operation_generation(operation_generation);
        let gateway_unlock = self.gateway_unlock_continuation();
        let active_wallet_generation = self.active_wallet_generation;
        let selected_chain = self.selected_chain;
        let base_profile_uuid = pending.base_profile_uuid.clone();
        let join = self.runtime.spawn_blocking(move || {
            let result = store.match_software_context_with_spend_unlock_ref(
                pending.vault_view_unlock.as_ref(),
                &pending.base_metadata,
                &pending.spend_unlock,
                passphrase.as_str(),
                pending.vault_session_id,
            )?;
            match result {
                wallet_ops::vault::SoftwareContextMatch::Known {
                    metadata,
                    session: protected_seed_session,
                } => {
                    let context_wallet_uuid = metadata.wallet_uuid.clone();
                    let session = store.load_view_session_with_view_unlock(
                        pending.vault_view_unlock.as_ref(),
                        &context_wallet_uuid,
                    )?;
                    let metadata = store.list_wallet_metadata_with_view_unlock(
                        pending.vault_view_unlock.as_ref(),
                        true,
                    )?;
                    Ok(PendingPassphraseTaskResult::Known {
                        session,
                        metadata,
                        protected_seed_session,
                    })
                }
                wallet_ops::vault::SoftwareContextMatch::Unknown => Ok(
                    PendingPassphraseTaskResult::Unknown(pending.into_unknown_decision(passphrase)),
                ),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.pending_software_profile_open_is_current(
                    operation_generation,
                    active_wallet_generation,
                    selected_chain,
                    base_profile_uuid.as_ref(),
                ) {
                    return;
                }
                match result {
                    Ok(Ok(PendingPassphraseTaskResult::Known {
                        session,
                        metadata,
                        protected_seed_session,
                    })) => {
                        let context_is_current = metadata.iter().any(|metadata| {
                            metadata.wallet_uuid == session.wallet_id()
                                && metadata.software_context.as_ref().is_some_and(|context| {
                                    context.kind == WalletSoftwareContextKind::Passphrase
                                        && context.base_profile_uuid == base_profile_uuid.as_ref()
                                })
                        });
                        if !context_is_current {
                            root.abandon_pending_software_profile_open(window, cx);
                            return;
                        }
                        root.pending_software_profile_open = None;
                        root.install_verified_software_context(
                            session,
                            &metadata,
                            Some(protected_seed_session),
                            gateway_unlock,
                            window,
                            cx,
                        );
                    }
                    Ok(Ok(PendingPassphraseTaskResult::Unknown(pending))) => {
                        root.pending_software_profile_open = Some(pending);
                        root.vault_error = None;
                        cx.notify();
                    }
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "passphrase matching task failed");
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn retry_pending_software_passphrase(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(pending) = self.pending_software_profile_open.as_mut()
            && pending.stage() == PendingSoftwareProfileOpenStage::UnknownDecision
        {
            pending.retry();
            self.vault_error = None;
            cx.notify();
        }
    }

    pub(in crate::root) fn begin_pending_software_context_creation(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .pending_software_profile_open
            .as_mut()
            .is_some_and(PendingSoftwareProfileOpen::enter_creation_handoff)
        {
            self.vault_error = None;
            cx.notify();
        }
    }

    pub(in crate::root) fn validate_pending_software_context_creation(
        &self,
        confirmation: &str,
        label: &str,
    ) -> Result<String, &'static str> {
        let Some(pending) = self.pending_software_profile_open.as_ref() else {
            return Err("Passphrase wallet creation is no longer available");
        };
        if !pending.passphrase_matches_confirmation(confirmation) {
            return Err("The passphrase entries do not match");
        }
        validate_pending_context_label(label, &pending.internal_metadata)
    }

    pub(in crate::root) fn cancel_pending_software_profile_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending_software_profile_open.is_some()
            || matches!(self.vault_state, VaultState::PendingSoftwareProfileOpen)
        {
            self.abandon_pending_software_profile_open(window, cx);
        }
    }

    pub(in crate::root) fn begin_open_passphrase_wallet(
        &mut self,
        target_base_profile_uuid: Arc<str>,
        vault_password: Zeroizing<String>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(metadata) = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == target_base_profile_uuid.as_ref())
        else {
            return;
        };
        let Some(context) = metadata.software_context.as_ref() else {
            return;
        };
        if context.kind != WalletSoftwareContextKind::Standard
            || context.base_profile_uuid != target_base_profile_uuid.as_ref()
        {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        let Some(vault_view_unlock) = self.vault_view_unlock.clone() else {
            self.set_vault_error(
                "Unlock the wallet vault before opening a passphrase wallet",
                cx,
            );
            return;
        };
        let Some(active_wallet_id) = self
            .view_session
            .as_ref()
            .map(|session| Arc::<str>::from(session.wallet_id().to_owned()))
        else {
            return;
        };
        let active_wallet_generation = self.active_wallet_generation;
        let selected_chain = self.selected_chain;
        let task_base_profile_uuid = target_base_profile_uuid;
        let authorization_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = authorization_generation;
        let join = self.runtime.spawn_blocking(move || {
            let mut grant = store.create_spend_grant(vault_password.as_str())?;
            let spend_unlock = grant.take_spend_unlock()?;
            let metadata =
                store.list_wallet_metadata_with_view_unlock(vault_view_unlock.as_ref(), true)?;
            let base_metadata = metadata
                .iter()
                .find(|metadata| {
                    metadata.wallet_uuid == task_base_profile_uuid.as_ref()
                        && metadata.status == wallet_ops::vault::WalletStatus::Active
                        && !metadata.source.is_hardware_derived()
                        && metadata.software_context.as_ref().is_some_and(|context| {
                            context.kind == WalletSoftwareContextKind::Standard
                                && context.base_profile_uuid == task_base_profile_uuid.as_ref()
                        })
                })
                .cloned()
                .ok_or(wallet_ops::vault::VaultError::WalletNotFound)?;
            let vault_session_id = VaultSessionId::random()?;
            PendingSoftwareProfileOpen::new(
                base_metadata,
                metadata,
                vault_view_unlock,
                spend_unlock,
                vault_session_id,
                0,
            )
            .ok_or(wallet_ops::vault::VaultError::InvalidWalletMetadata)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.is_active_wallet_generation(
                    active_wallet_id.as_ref(),
                    active_wallet_generation,
                )
                    || root.pending_software_profile_open_operation_generation
                        != authorization_generation
                    || root.selected_chain != selected_chain
                    || !matches!(root.vault_state, VaultState::ViewUnlocked)
                {
                    return;
                }
                match result {
                    Ok(Ok(pending)) => root.enter_pending_software_profile_open(pending, None, window, cx),
                    Ok(Err(error)) => root.handle_vault_error(&error, cx),
                    Err(error) => {
                        tracing::warn!(%error, "fresh passphrase authorization task failed");
                        root.set_vault_error(
                            "Passphrase wallet authorization failed. Check the vault password and try again.",
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn create_pending_software_context(
        &mut self,
        passphrase_confirmation: Zeroizing<String>,
        label: String,
        intent: SoftwareContextSyncIntent,
        chains: Vec<SoftwareContextChainInput>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(pending) = self.pending_software_profile_open.take() else {
            return;
        };
        let mut pending = pending;
        let Some(passphrase) = pending.take_for_creation_passphrase() else {
            self.pending_software_profile_open = Some(pending);
            return;
        };
        let Some(store) = self.vault_store.clone() else {
            self.abandon_pending_software_profile_open(window, cx);
            return;
        };
        let operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = operation_generation;
        pending.set_operation_generation(operation_generation);
        let gateway_unlock = self.gateway_unlock_continuation();
        let active_wallet_generation = self.active_wallet_generation;
        let selected_chain = self.selected_chain;
        let base_profile_uuid = pending.base_profile_uuid.clone();
        let task_base_profile_uuid = base_profile_uuid.clone();
        let join = self.runtime.spawn_blocking(move || {
            let context_wallet_uuid = wallet_ops::vault::generate_opaque_id()?;
            let result = store.create_software_context_with_spend_unlock(
                pending.vault_view_unlock.as_ref(),
                &pending.spend_unlock,
                task_base_profile_uuid.as_ref(),
                &context_wallet_uuid,
                pending.base_metadata.derivation_index,
                &label,
                passphrase,
                passphrase_confirmation,
                intent,
                &chains,
                pending.vault_session_id,
            )?;
            let (context_metadata, protected_seed_session) = match result {
                CreateSoftwareContextResult::ExistingContext {
                    metadata,
                    protected_seed_session,
                }
                | CreateSoftwareContextResult::Created {
                    metadata,
                    protected_seed_session,
                    ..
                } => (metadata, protected_seed_session),
            };
            let session = store.load_view_session_with_view_unlock(
                pending.vault_view_unlock.as_ref(),
                &context_metadata.wallet_uuid,
            )?;
            let metadata = store
                .list_wallet_metadata_with_view_unlock(pending.vault_view_unlock.as_ref(), true)?;
            Ok::<_, wallet_ops::vault::VaultError>((session, metadata, protected_seed_session))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.pending_software_profile_open_is_current(
                    operation_generation,
                    active_wallet_generation,
                    selected_chain,
                    base_profile_uuid.as_ref(),
                ) {
                    return;
                }
                root.pending_software_profile_open = None;
                match result {
                    Ok(Ok((session, metadata, protected_seed_session))) => {
                        let context_is_current = metadata.iter().any(|metadata| {
                            metadata.wallet_uuid == session.wallet_id()
                                && metadata.software_context.as_ref().is_some_and(|context| {
                                    context.kind == WalletSoftwareContextKind::Passphrase
                                        && context.base_profile_uuid == base_profile_uuid.as_ref()
                                })
                        });
                        if context_is_current {
                            root.install_verified_software_context(
                                session,
                                &metadata,
                                Some(protected_seed_session),
                                gateway_unlock,
                                window,
                                cx,
                            );
                        } else {
                            root.abandon_pending_software_profile_open(window, cx);
                        }
                    }
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "software context creation task failed");
                        root.abandon_pending_software_profile_open(window, cx);
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn prepare_pending_software_context_creation(
        &mut self,
        passphrase_confirmation: Zeroizing<String>,
        label: &str,
        intent: SoftwareContextSyncIntent,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (label, base_profile_uuid) = {
            let Some(pending) = self.pending_software_profile_open.as_ref() else {
                return;
            };
            if pending.stage() != PendingSoftwareProfileOpenStage::CreationHandoff {
                return;
            }
            if passphrase_confirmation.is_empty() {
                self.vault_error = Some(Arc::from("Enter the mnemonic passphrase again"));
                cx.notify();
                return;
            }
            if !pending.passphrase_matches_confirmation(passphrase_confirmation.as_str()) {
                self.vault_error = Some(Arc::from("The passphrase entries do not match"));
                cx.notify();
                return;
            }
            let label = match validate_pending_context_label(label, &pending.internal_metadata) {
                Ok(label) => label,
                Err(error) => {
                    self.vault_error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            };
            (label, pending.base_profile_uuid.clone())
        };
        self.vault_error = None;

        let operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = operation_generation;
        if let Some(pending) = self.pending_software_profile_open.as_mut() {
            pending.set_operation_generation(operation_generation);
        }
        let active_wallet_generation = self.active_wallet_generation;
        let selected_chain = self.selected_chain;
        let effective_chains = self
            .effective_chain_configs
            .values()
            .filter(|chain| chain.enabled)
            .cloned()
            .collect::<Vec<_>>();
        let http = self.http.clone();
        let join = self.runtime.spawn(async move {
            let mut chains = Vec::with_capacity(effective_chains.len());
            for effective_chain in effective_chains {
                let current_safe_head = if intent == SoftwareContextSyncIntent::CreateNew {
                    Some(wallet_ops::fetch_current_safe_head(&effective_chain, &http).await?)
                } else {
                    None
                };
                if creation_chain_baseline(
                    intent,
                    effective_chain.deployment_block,
                    current_safe_head,
                )
                .is_none()
                {
                    return Err(eyre::eyre!(
                        "software context chain baseline is unavailable"
                    ));
                }
                chains.push(SoftwareContextChainInput {
                    chain_type: 0,
                    chain_id: effective_chain.chain_id,
                    contract: effective_chain.railgun_contract,
                    deployment_block: effective_chain.deployment_block,
                    current_safe_head,
                });
            }
            Ok::<_, eyre::Report>(chains)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.pending_software_profile_open_is_current(
                    operation_generation,
                    active_wallet_generation,
                    selected_chain,
                    base_profile_uuid.as_ref(),
                ) {
                    return;
                }
                match result {
                    Ok(Ok(chains)) => root.create_pending_software_context(
                        passphrase_confirmation,
                        label,
                        intent,
                        chains,
                        window,
                        cx,
                    ),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "software context chain baseline preparation failed");
                        root.vault_error = Some(Arc::from(
                            "Current chain safe heads are unavailable. Check the network and retry.",
                        ));
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "software context chain baseline task failed");
                        root.vault_error = Some(Arc::from(
                            "Current chain safe heads are unavailable. Check the network and retry.",
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingSoftwareProfileOpen {
        let created = wallet_ops::vault::create_with_params(
            "test-vault-password",
            wallet_ops::vault::KdfParams::default(),
        )
        .expect("create test vault");
        let metadata = WalletMetadataBundle {
            wallet_uuid: "base-profile".to_owned(),
            label: "Base".to_owned(),
            derivation_index: 0,
            source: wallet_ops::vault::WalletSource::Imported,
            status: wallet_ops::vault::WalletStatus::Active,
            display_order: 0,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids: std::collections::BTreeSet::default(),
            software_context: Some(wallet_ops::vault::WalletSoftwareContext::standard(
                "base-profile",
            )),
        };
        PendingSoftwareProfileOpen::new(
            metadata.clone(),
            vec![metadata],
            Arc::new(created.view),
            created.spend,
            VaultSessionId::from_bytes([4; 16]),
            1,
        )
        .expect("pending profile")
    }

    #[test]
    fn pending_profile_open_has_bounded_retry_and_creation_stages() {
        let mut pending = pending();
        assert_eq!(pending.stage(), PendingSoftwareProfileOpenStage::Choosing);

        pending = pending.into_unknown_decision(Zeroizing::new("first".to_owned()));
        assert_eq!(
            pending.stage(),
            PendingSoftwareProfileOpenStage::UnknownDecision
        );
        assert!(!pending.passphrase_matches_confirmation("second"));
        assert!(pending.enter_creation_handoff());
        assert!(pending.passphrase_matches_confirmation("first"));
        assert!(!pending.passphrase_matches_confirmation("first "));
        pending.retry();
        assert_eq!(pending.stage(), PendingSoftwareProfileOpenStage::Choosing);
        assert!(!pending.passphrase_matches_confirmation("first"));

        pending = pending.into_unknown_decision(Zeroizing::new("first".to_owned()));
        let (pending, first) = {
            let first = pending
                .take_for_creation_passphrase()
                .expect("first passphrase");
            (pending, first)
        };
        assert_eq!(first.as_str(), "first");
        assert_eq!(
            pending.stage(),
            PendingSoftwareProfileOpenStage::CreationHandoff
        );
    }
}
