use std::sync::Arc;

use gpui::{Context, WeakEntity};
use tokio::runtime::Handle;
use wallet_ops::vault::DesktopVaultStore;

use super::{WalletRoot, chain_load::WalletRootReplacementCleanup};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::root) enum WalletMaintenanceReset {
    #[default]
    Idle,
    Public,
    Poi,
    Merkle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicSyncResetCompletion {
    CleanupFailed,
    ResetAttempted,
}

pub(in crate::root) const fn public_sync_reset_restart_is_safe(
    resync_requested: bool,
    completion: PublicSyncResetCompletion,
) -> bool {
    resync_requested && matches!(completion, PublicSyncResetCompletion::ResetAttempted)
}

#[derive(Debug, Default)]
pub(in crate::root) struct WalletMaintenanceStateMachine {
    reset: WalletMaintenanceReset,
    generation: u64,
    status: Option<Arc<str>>,
}

impl WalletMaintenanceStateMachine {
    pub(in crate::root) const fn reset(&self) -> WalletMaintenanceReset {
        self.reset
    }

    pub(in crate::root) fn status(&self) -> Option<Arc<str>> {
        self.status.clone()
    }

    pub(in crate::root) fn try_acquire(
        &mut self,
        reset: WalletMaintenanceReset,
        status: impl Into<Arc<str>>,
    ) -> Option<u64> {
        if self.reset != WalletMaintenanceReset::Idle || reset == WalletMaintenanceReset::Idle {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.reset = reset;
        self.status = Some(status.into());
        Some(self.generation)
    }

    pub(in crate::root) fn complete(
        &mut self,
        generation: u64,
        status: impl Into<Arc<str>>,
    ) -> bool {
        if self.reset == WalletMaintenanceReset::Idle || self.generation != generation {
            return false;
        }
        self.reset = WalletMaintenanceReset::Idle;
        self.status = Some(status.into());
        true
    }

    fn clear_status(&mut self) -> bool {
        if self.reset != WalletMaintenanceReset::Idle {
            return false;
        }
        self.status.take().is_some()
    }

    pub(in crate::root) fn set_idle_status(&mut self, status: impl Into<Arc<str>>) -> bool {
        if self.reset != WalletMaintenanceReset::Idle {
            return false;
        }
        self.status = Some(status.into());
        true
    }
}

pub(in crate::root) struct WalletMaintenanceController {
    runtime: Handle,
    state: WalletMaintenanceStateMachine,
    active_root: Option<WeakEntity<WalletRoot>>,
    root_replacement_cleanup: Option<WalletRootReplacementCleanup>,
}

impl WalletMaintenanceController {
    pub(in crate::root) fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            state: WalletMaintenanceStateMachine::default(),
            active_root: None,
            root_replacement_cleanup: None,
        }
    }

    pub(in crate::root) const fn reset(&self) -> WalletMaintenanceReset {
        self.state.reset()
    }

    pub(in crate::root) fn is_idle(&self) -> bool {
        self.reset() == WalletMaintenanceReset::Idle && !self.root_replacement_cleanup_in_progress()
    }

    pub(in crate::root) fn status(&self) -> Option<Arc<str>> {
        self.state.status()
    }

    pub(in crate::root) fn set_active_root(&mut self, active_root: WeakEntity<WalletRoot>) {
        self.active_root = Some(active_root);
    }

    pub(in crate::root) fn clear_active_root(&mut self) {
        self.active_root = None;
    }

    pub(in crate::root) fn set_root_replacement_cleanup(
        &mut self,
        cleanup: Option<WalletRootReplacementCleanup>,
    ) {
        self.root_replacement_cleanup = cleanup;
    }

    pub(in crate::root) fn clear_finished_root_replacement_cleanup(&mut self) {
        if self
            .root_replacement_cleanup
            .as_ref()
            .is_some_and(WalletRootReplacementCleanup::is_finished)
        {
            self.root_replacement_cleanup = None;
        }
    }

    const fn root_replacement_cleanup_in_progress(&self) -> bool {
        self.root_replacement_cleanup.is_some()
    }

    pub(in crate::root) fn clear_status(&mut self, cx: &mut Context<'_, Self>) {
        if self.state.clear_status() {
            cx.notify();
        }
    }

    pub(in crate::root) fn start_public_reset(
        &mut self,
        vault_store: &DesktopVaultStore,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.root_replacement_cleanup_in_progress() {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting caches");
            cx.notify();
            return false;
        }
        if self.active_root.as_ref().is_some_and(|root| {
            !root
                .update(cx, |root, _cx| root.destructive_cache_reset_is_allowed())
                .unwrap_or(false)
        }) {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting caches");
            cx.notify();
            return false;
        }
        let Some(generation) = self.state.try_acquire(
            WalletMaintenanceReset::Public,
            "Resetting public sync caches...",
        ) else {
            return false;
        };
        let reset_context = self.active_root.as_ref().and_then(|root| {
            root.update(cx, WalletRoot::begin_public_sync_cache_reset)
                .ok()
        });
        let db = vault_store.db();
        let resync_requested = reset_context.is_some();
        let join = self.runtime.spawn(async move {
            let store = match reset_context {
                Some(reset_context) => match reset_context.shutdown_for_public_reset().await {
                    Ok(store) => store,
                    Err(error) => {
                        return (Err(error), PublicSyncResetCompletion::CleanupFailed);
                    }
                },
                None => None,
            };
            let result = if let Some(store) = store {
                let report = match store.reset_public_sync_caches().await {
                    Ok(report) => report,
                    Err(error) => {
                        return (
                            Err(format!("public cache reset admission failed: {error}")),
                            PublicSyncResetCompletion::ResetAttempted,
                        );
                    }
                };
                if let Err(error) = report.persisted.as_ref() {
                    Err(format!("persisted public cache reset failed: {error}"))
                } else if report.failed_chain_count() > 0 {
                    let failed = report.failed_chain_count();
                    let first_failure = report
                        .chains
                        .iter()
                        .find_map(|reset| {
                            reset.result.as_ref().err().map(|error| {
                                format!(
                                    "chain {} contract {}: {error}",
                                    reset.chain.chain_id, reset.chain.contract
                                )
                            })
                        })
                        .expect("failed reset report contains an error");
                    Err(format!(
                        "{failed} of {} chain resets failed; first failure: {first_failure}",
                        report.chains.len()
                    ))
                } else {
                    Ok(report.total_removed_entries)
                }
            } else {
                wallet_ops::reset_persisted_public_sync_caches(db.as_ref())
                    .await
                    .map(wallet_ops::PersistedPublicSyncCacheResetReport::total_removed_entries)
                    .map_err(|error| error.to_string())
            };
            (result, PublicSyncResetCompletion::ResetAttempted)
        });
        cx.spawn(async move |this, cx| {
            let (message, restart_safe) = match join.await {
                Ok((Ok(removed), completion)) if resync_requested => (
                    format!(
                        "Public sync caches reset; cleared {removed} cache records and requested resync"
                    ),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Ok((Ok(removed), completion)) => (
                    format!("Persisted public sync caches reset; cleared {removed} cache records"),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Ok((Err(error), completion)) => (
                    format!("Failed to reset public sync caches: {error}"),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Err(error) => (
                    format!("Public sync cache reset task failed: {error}"),
                    false,
                ),
            };
            let _ = this.update(cx, |controller, cx| {
                if controller.state.complete(generation, message) {
                    if let Some(root) = controller.active_root.as_ref() {
                        let _ = root.update(cx, |root, cx| {
                            root.finish_public_sync_cache_reset(restart_safe, cx);
                        });
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
        true
    }

    pub(in crate::root) fn start_poi_reset(
        &mut self,
        vault_store: &DesktopVaultStore,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.root_replacement_cleanup_in_progress() {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting PPOI data");
            cx.notify();
            return false;
        }
        if self.active_root.as_ref().is_some_and(|root| {
            !root
                .update(cx, |root, _cx| root.destructive_cache_reset_is_allowed())
                .unwrap_or(false)
        }) {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting PPOI data");
            cx.notify();
            return false;
        }
        let Some(generation) = self
            .state
            .try_acquire(WalletMaintenanceReset::Poi, "Resetting PPOI data...")
        else {
            return false;
        };
        let reset_context = self
            .active_root
            .as_ref()
            .and_then(|root| root.update(cx, WalletRoot::begin_poi_data_reset).ok());
        let db = vault_store.db();
        let resync_requested = reset_context.is_some();
        let join = self.runtime.spawn(async move {
            if let Some(reset_context) = reset_context
                && let Err(error) = reset_context.shutdown_for_poi_reset().await
            {
                return (Err(error), PublicSyncResetCompletion::CleanupFailed);
            }
            let result = wallet_ops::reset_persisted_poi_data(db.as_ref())
                .await
                .map_err(|error| error.to_string());
            (result, PublicSyncResetCompletion::ResetAttempted)
        });
        cx.spawn(async move |this, cx| {
            let (message, restart_safe) = match join.await {
                Ok((Ok(report), completion)) if resync_requested => (
                    format!(
                        "PPOI data reset; removed {} data records and {} downloaded chunks; rebuilding",
                        report.data_records_removed, report.chunk_records_removed
                    ),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Ok((Ok(report), completion)) => (
                    format!(
                        "Persisted PPOI data reset; removed {} data records and {} downloaded chunks",
                        report.data_records_removed, report.chunk_records_removed
                    ),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Ok((Err(error), completion)) => (
                    format!("Failed to reset PPOI data: {error}"),
                    public_sync_reset_restart_is_safe(resync_requested, completion),
                ),
                Err(error) => (format!("PPOI data reset task failed: {error}"), false),
            };
            let _ = this.update(cx, |controller, cx| {
                if controller.state.complete(generation, message) {
                    if let Some(root) = controller.active_root.as_ref() {
                        let _ = root.update(cx, |root, cx| {
                            root.finish_public_sync_cache_reset(restart_safe, cx);
                        });
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
        true
    }

    pub(in crate::root) fn start_merkle_reset(
        &mut self,
        vault_store: &DesktopVaultStore,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.root_replacement_cleanup_in_progress() {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting caches");
            cx.notify();
            return false;
        }
        if self.active_root.as_ref().is_some_and(|root| {
            !root
                .update(cx, |root, _cx| root.destructive_cache_reset_is_allowed())
                .unwrap_or(false)
        }) {
            self.state
                .set_idle_status("Wait for wallet sync cleanup before resetting caches");
            cx.notify();
            return false;
        }
        let Some(generation) = self.state.try_acquire(
            WalletMaintenanceReset::Merkle,
            "Resetting local Merkle forest cache...",
        ) else {
            return false;
        };
        let active_root = self.active_root.clone();
        let cleanup = active_root.as_ref().and_then(|root| {
            root.update(cx, WalletRoot::begin_merkle_forest_cache_reset)
                .ok()
        });
        let resync_requested = cleanup.is_some();
        let db = vault_store.db();
        let join = self.runtime.spawn(async move {
            if let Some(cleanup) = cleanup {
                cleanup.shutdown_for_merkle_reset().await?;
            }
            tokio::task::spawn_blocking(move || {
                wallet_ops::reset_local_merkle_forest_cache(db.as_ref())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        });
        cx.spawn(async move |this, cx| {
            let (message, reset_succeeded) = match join.await {
                Ok(Ok(removed)) if resync_requested => (
                    format!(
                        "Local Merkle forest cache reset; cleared {removed} snapshot files and restarted private sync"
                    ),
                    true,
                ),
                Ok(Ok(removed)) => (
                    format!(
                        "Local Merkle forest cache reset; cleared {removed} snapshot files"
                    ),
                    true,
                ),
                Ok(Err(error)) => (
                    format!("Failed to reset local Merkle forest cache: {error}"),
                    false,
                ),
                Err(error) => (
                    format!("Local Merkle forest cache reset task failed: {error}"),
                    false,
                ),
            };
            let _ = this.update(cx, |controller, cx| {
                if !controller.state.complete(generation, message) {
                    return;
                }
                if let Some(root) = controller.active_root.as_ref() {
                    let _ = root.update(cx, |root, cx| {
                        root.finish_merkle_forest_cache_reset(reset_succeeded, cx);
                    });
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        true
    }
}
