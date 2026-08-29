use std::sync::Arc;

use gpui::{
    App, AppContext, Context, ElementId, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use ui::controls::{app_button, app_input, app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::settings::RememberedWalletKind;
use wallet_ops::vault::{
    DesktopVaultStore, DesktopViewSession, VaultError, ViewUnlock, WalletMetadataBundle,
    WalletSoftwareContextKind, WalletSource, WalletStatus, sort_wallet_metadata,
};

use crate::assets::RailgunActionIcon;

use super::vault::{
    WalletOption, vault_error_kind, visible_wallet_metadata, wallet_options_from_metadata,
};
use super::{
    APP_TEXT_SIZE, ConfirmationDialogProps, WalletRoot,
    chain_load::WalletSyncLifecycleCleanupWaitGroup, confirmation_dialog,
    dialog_content_max_height, dialog_max_height, participant::remove_private_wallet_participants,
    scrollable_dialog_content, secondary_dialog_content_width,
};

#[derive(Default)]
pub(super) struct ManageWalletsState {
    pub(super) editing_wallet_id: Option<Arc<str>>,
    pub(super) pending_delete_wallet_id: Option<Arc<str>>,
    pub(super) deleting_wallet_id: Option<Arc<str>>,
    pub(super) hardware_delete_wallet_id: Option<Arc<str>>,
    hardware_delete_unlock_lifecycle: HardwareDeleteUnlockLifecycle,
    pub(super) error: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
enum HardwareDeleteUnlockLifecycle {
    #[default]
    Inactive,
    AwaitingTarget,
    TargetOpened,
}

impl ManageWalletsState {
    pub(super) const fn root_replacement_is_allowed(&self) -> bool {
        wallet_root_replacement_is_allowed(self.deleting_wallet_id.is_some())
    }

    pub(super) fn reset_for_open(&mut self) {
        let deleting_wallet_id = self.deleting_wallet_id.clone();
        let hardware_delete_wallet_id = self.hardware_delete_wallet_id.clone();
        let hardware_delete_unlock_lifecycle = self.hardware_delete_unlock_lifecycle;
        *self = Self {
            deleting_wallet_id,
            hardware_delete_wallet_id,
            hardware_delete_unlock_lifecycle,
            ..Self::default()
        };
    }

    #[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
    pub(super) fn begin_hardware_delete_unlock(&mut self, wallet_id: Arc<str>) {
        self.hardware_delete_wallet_id = Some(wallet_id);
        self.hardware_delete_unlock_lifecycle = HardwareDeleteUnlockLifecycle::AwaitingTarget;
    }

    #[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
    pub(super) fn mark_hardware_delete_target_opened(&mut self, wallet_id: &str) {
        if self.hardware_delete_unlock_lifecycle == HardwareDeleteUnlockLifecycle::AwaitingTarget
            && self.hardware_delete_wallet_id.as_deref() == Some(wallet_id)
        {
            self.hardware_delete_unlock_lifecycle = HardwareDeleteUnlockLifecycle::TargetOpened;
        }
    }

    #[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
    pub(super) fn finish_hardware_delete_unlock_dialog(&mut self) {
        if self.hardware_delete_unlock_lifecycle == HardwareDeleteUnlockLifecycle::AwaitingTarget {
            self.hardware_delete_wallet_id = None;
        }
        self.hardware_delete_unlock_lifecycle = HardwareDeleteUnlockLifecycle::Inactive;
    }

    fn clear_hardware_delete_unlock_intent(&mut self) {
        self.hardware_delete_wallet_id = None;
        self.hardware_delete_unlock_lifecycle = HardwareDeleteUnlockLifecycle::Inactive;
    }
}

enum WalletDeletionTaskError {
    SyncShutdown(String),
    Join(String),
    Vault(VaultError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WalletManagementSelection {
    KeepSelected,
    SwitchTo(Arc<str>),
    NoActiveWallet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletManagementDeleteKind {
    Wallet,
    SoftwareProfile,
}

#[derive(Clone)]
struct WalletManagementDrag {
    wallet_id: Arc<str>,
    label: Arc<str>,
}

struct WalletManagementDragPreview {
    label: Arc<str>,
    position: Point<Pixels>,
}

impl Render for WalletManagementDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .pl(self.position.x)
            .pt(self.position.y)
            .px(px(10.0))
            .py(px(6.0))
            .rounded_md()
            .bg(rgb(theme::SELECTED_SURFACE))
            .border_1()
            .border_color(rgb(theme::PRIMARY))
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT))
            .child(SharedString::from(self.label.to_string()))
    }
}

pub(super) fn active_wallet_management_rows(
    metadata: &[WalletMetadataBundle],
) -> Vec<WalletMetadataBundle> {
    let mut rows = metadata
        .iter()
        .filter(|metadata| metadata.status == WalletStatus::Active)
        .cloned()
        .collect::<Vec<_>>();
    sort_wallet_metadata(&mut rows);
    rows
}

pub(super) fn hidden_wallet_management_rows(
    metadata: &[WalletMetadataBundle],
) -> Vec<WalletMetadataBundle> {
    let mut rows = metadata
        .iter()
        .filter(|metadata| metadata.status == WalletStatus::Inactive)
        .cloned()
        .collect::<Vec<_>>();
    sort_wallet_metadata(&mut rows);
    rows
}

pub(super) fn wallet_ids_after_drop(
    active_wallet_ids: &[Arc<str>],
    dragged_wallet_id: &str,
    drop_index: usize,
) -> Option<Vec<String>> {
    let original = active_wallet_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let old_index = original
        .iter()
        .position(|wallet_id| wallet_id == dragged_wallet_id)?;
    let mut reordered = original.clone();
    let dragged = reordered.remove(old_index);
    let mut insert_index = drop_index.min(active_wallet_ids.len());
    if old_index < insert_index {
        insert_index = insert_index.saturating_sub(1);
    }
    insert_index = insert_index.min(reordered.len());
    reordered.insert(insert_index, dragged);
    (reordered != original).then_some(reordered)
}

pub(super) fn selected_wallet_after_metadata_refresh(
    selected_wallet_id: Option<&str>,
    options: &[WalletOption],
) -> WalletManagementSelection {
    if selected_wallet_id.is_some_and(|selected| {
        options
            .iter()
            .any(|option| option.wallet_id.as_ref() == selected)
    }) {
        return WalletManagementSelection::KeepSelected;
    }

    options
        .first()
        .map_or(WalletManagementSelection::NoActiveWallet, |option| {
            WalletManagementSelection::SwitchTo(Arc::clone(&option.wallet_id))
        })
}

pub(super) fn wallet_management_switch_requires_device(
    wallet_id: &str,
    options: &[WalletOption],
) -> bool {
    options
        .iter()
        .any(|option| option.wallet_id.as_ref() == wallet_id && option.source.is_hardware_derived())
}

pub(super) fn wallet_management_delete_requires_device(
    wallet_id: &str,
    source: WalletSource,
    open_session_wallet_id: Option<&str>,
) -> bool {
    source.is_hardware_derived() && open_session_wallet_id != Some(wallet_id)
}

pub(super) const fn wallet_management_delete_uses_dialog(source: WalletSource) -> bool {
    !source.is_hardware_derived()
}

pub(super) fn wallet_management_delete_kind(
    target: &WalletMetadataBundle,
    metadata: &[WalletMetadataBundle],
) -> WalletManagementDeleteKind {
    let Some(context) = target.software_context.as_ref() else {
        return WalletManagementDeleteKind::Wallet;
    };
    if target.source.is_hardware_derived()
        || context.kind != WalletSoftwareContextKind::Standard
        || context.base_profile_uuid != target.wallet_uuid
    {
        return WalletManagementDeleteKind::Wallet;
    }
    if metadata.iter().any(|metadata| {
        metadata.software_context.as_ref().is_some_and(|context| {
            context.kind == WalletSoftwareContextKind::Passphrase
                && context.base_profile_uuid == target.wallet_uuid
        })
    }) {
        WalletManagementDeleteKind::SoftwareProfile
    } else {
        WalletManagementDeleteKind::Wallet
    }
}

pub(super) fn wallet_management_deletion_affects_selected_wallet(
    target: &WalletMetadataBundle,
    delete_kind: WalletManagementDeleteKind,
    selected_wallet_id: Option<&str>,
    metadata: &[WalletMetadataBundle],
) -> bool {
    let Some(selected_wallet_id) = selected_wallet_id else {
        return false;
    };
    if selected_wallet_id == target.wallet_uuid {
        return true;
    }
    delete_kind == WalletManagementDeleteKind::SoftwareProfile
        && metadata.iter().any(|metadata| {
            metadata.wallet_uuid == selected_wallet_id
                && metadata.software_context.as_ref().is_some_and(|context| {
                    context.kind == WalletSoftwareContextKind::Passphrase
                        && context.base_profile_uuid == target.wallet_uuid
                })
        })
}

pub(super) fn wallet_management_preserves_hidden_delete_session(
    hardware_delete_wallet_id: Option<&str>,
    selected_wallet_id: Option<&str>,
    open_session_wallet_id: Option<&str>,
) -> bool {
    hardware_delete_wallet_id.is_some()
        && hardware_delete_wallet_id == selected_wallet_id
        && hardware_delete_wallet_id == open_session_wallet_id
}

pub(super) const fn restart_selected_wallet_sync_after_deletion(
    deleting_selected_wallet: bool,
    deletion_succeeded: bool,
) -> bool {
    deleting_selected_wallet && !deletion_succeeded
}

const fn wallet_root_replacement_is_allowed(wallet_deletion_in_progress: bool) -> bool {
    !wallet_deletion_in_progress
}

impl WalletRoot {
    pub(in crate::root) const fn root_replacement_is_allowed(&self) -> bool {
        self.manage_wallets.root_replacement_is_allowed()
    }

    pub(super) fn open_manage_wallets_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.refresh_wallet_management_metadata(window, cx) {
            return;
        }
        window.close_all_dialogs(cx);
        self.manage_wallets.reset_for_open();
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(680.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Manage wallets"))
                .child(scrollable_dialog_content(
                    content_max_height,
                    content_root
                        .read(cx)
                        .render_manage_wallets_dialog_content(&content_root, content_width),
                ))
        });
    }

    pub(super) fn save_wallet_label_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(wallet_id) = self.manage_wallets.editing_wallet_id.clone() else {
            return;
        };
        let label = self.manage_wallet_label_input.read(cx).value().to_string();
        self.run_wallet_management_mutation(window, cx, move |store, view| {
            store
                .update_wallet_label_with_view_unlock(view, wallet_id.as_ref(), &label)
                .map(|_| ())
        });
        if self.manage_wallets.error.is_none() {
            self.manage_wallets.editing_wallet_id = None;
        }
    }

    fn begin_wallet_label_edit(
        &mut self,
        wallet_id: Arc<str>,
        label: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.manage_wallets.editing_wallet_id = Some(wallet_id);
        self.manage_wallets.pending_delete_wallet_id = None;
        self.manage_wallets.error = None;
        self.manage_wallet_label_input.update(cx, |input, cx| {
            input.set_value(label.to_owned(), window, cx);
        });
        self.manage_wallet_label_input
            .read(cx)
            .focus_handle(cx)
            .focus(window);
        cx.notify();
    }

    fn cancel_wallet_label_edit(&mut self, cx: &mut Context<'_, Self>) {
        self.manage_wallets.editing_wallet_id = None;
        self.manage_wallets.error = None;
        cx.notify();
    }

    fn set_wallet_visibility_from_dialog(
        &mut self,
        wallet_id: Arc<str>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_wallet_management_mutation(window, cx, move |store, view| {
            store
                .set_wallet_active_with_view_unlock(view, wallet_id.as_ref(), active)
                .map(|_| ())
        });
    }

    fn delete_wallet_from_dialog(
        &mut self,
        wallet_id: Arc<str>,
        software_delete_confirmed: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.set_wallet_management_error(
                "Wait for the active cache reset before deleting a wallet",
                cx,
            );
            return;
        }
        if self.manage_wallets.deleting_wallet_id.is_some() {
            return;
        }
        let Some(target) = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref())
            .cloned()
        else {
            self.handle_wallet_management_error(&VaultError::WalletNotFound, cx);
            return;
        };
        if wallet_management_delete_requires_device(
            wallet_id.as_ref(),
            target.source,
            self.view_session
                .as_deref()
                .map(DesktopViewSession::wallet_id),
        ) {
            Self::open_inaccessible_hardware_delete_dialog(
                wallet_id,
                target.status == WalletStatus::Active,
                window,
                cx,
            );
            return;
        }
        if wallet_management_delete_uses_dialog(target.source) && !software_delete_confirmed {
            self.manage_wallets.pending_delete_wallet_id = None;
            self.manage_wallets.error = None;
            cx.notify();
            let root = cx.entity();
            let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
            let dialog_max_height = dialog_max_height(window);
            let content_max_height = dialog_content_max_height(window);
            window.open_dialog(cx, move |dialog, _window, _cx| {
                let confirm_root = root.clone();
                let confirm_wallet_id = Arc::clone(&wallet_id);
                confirmation_dialog(
                    dialog,
                    ConfirmationDialogProps::danger(
                        "Delete wallet?",
                        software_wallet_delete_confirmation_copy(),
                        None,
                        "Delete wallet",
                    ),
                    dialog_width,
                    dialog_max_height,
                    content_max_height,
                )
                .on_ok(move |_event, window, cx| {
                    let confirm_wallet_id = Arc::clone(&confirm_wallet_id);
                    confirm_root.update(cx, |root, cx| {
                        root.delete_wallet_from_dialog(confirm_wallet_id, true, window, cx);
                    });
                    true
                })
            });
            return;
        }
        let (delete_kind, full_metadata) = if target.source.is_hardware_derived() {
            (WalletManagementDeleteKind::Wallet, None)
        } else {
            let (store, view) = match self.wallet_management_context() {
                Ok(context) => context,
                Err(message) => {
                    self.set_wallet_management_error(message, cx);
                    return;
                }
            };
            let metadata = match store.list_wallet_metadata_with_view_unlock(view.as_ref(), true) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.handle_wallet_management_error(&error, cx);
                    return;
                }
            };
            (
                wallet_management_delete_kind(&target, &metadata),
                Some(metadata),
            )
        };
        if !wallet_management_delete_uses_dialog(target.source)
            && delete_kind == WalletManagementDeleteKind::Wallet
            && self.manage_wallets.pending_delete_wallet_id.as_deref() != Some(wallet_id.as_ref())
        {
            self.manage_wallets.pending_delete_wallet_id = Some(wallet_id);
            self.manage_wallets.error = None;
            cx.notify();
            return;
        }

        let target_session = self
            .view_session
            .clone()
            .filter(|session| session.wallet_id() == wallet_id.as_ref());
        let (store, view) = match self.wallet_management_context() {
            Ok(context) => context,
            Err(message) => {
                self.set_wallet_management_error(message, cx);
                return;
            }
        };
        let deleting_selected_wallet = wallet_management_deletion_affects_selected_wallet(
            &target,
            delete_kind,
            self.selected_wallet_id.as_deref(),
            full_metadata.as_deref().unwrap_or(&[]),
        );
        self.manage_wallets.deleting_wallet_id = Some(Arc::clone(&wallet_id));
        self.manage_wallets.error = None;
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let cleanup = if deleting_selected_wallet {
            self.begin_wallet_deletion_sync_shutdown(cx)
        } else {
            self.wallet_sync_cleanup_wait_group()
        };
        self.delete_wallet_after_sync_shutdown(
            wallet_id.as_ref(),
            target_session,
            store,
            view,
            deleting_selected_wallet,
            delete_kind == WalletManagementDeleteKind::SoftwareProfile,
            cleanup,
            window,
            cx,
        );
    }

    fn delete_wallet_after_sync_shutdown(
        &self,
        wallet_id: &str,
        target_session: Option<Arc<DesktopViewSession>>,
        store: Arc<DesktopVaultStore>,
        view: Arc<ViewUnlock>,
        deleting_selected_wallet: bool,
        whole_profile: bool,
        cleanup: WalletSyncLifecycleCleanupWaitGroup,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let wallet_id_string = wallet_id.to_owned();
        let join = self.runtime.spawn(async move {
            cleanup
                .shutdown_for_wallet_deletion()
                .await
                .map_err(WalletDeletionTaskError::SyncShutdown)?;
            tokio::task::spawn_blocking(move || {
                if whole_profile {
                    store
                        .delete_software_profile_with_view_unlock(view.as_ref(), &wallet_id_string)
                        .map(|wallets| {
                            wallets
                                .into_iter()
                                .map(|wallet| wallet.wallet_uuid)
                                .collect()
                        })
                } else {
                    match target_session.as_deref() {
                        Some(session) => {
                            store.delete_wallet_for_session(session, &wallet_id_string)
                        }
                        None => {
                            store.delete_wallet_with_view_unlock(view.as_ref(), &wallet_id_string)
                        }
                    }
                    .map(|wallet| vec![wallet.wallet_uuid])
                }
            })
            .await
            .map_err(|error| WalletDeletionTaskError::Join(error.to_string()))?
            .map_err(WalletDeletionTaskError::Vault)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.manage_wallets.deleting_wallet_id = None;
                let deletion_succeeded =
                    matches!(&result, Ok(Ok(wallet_ids)) if !wallet_ids.is_empty());
                if restart_selected_wallet_sync_after_deletion(
                    deleting_selected_wallet,
                    deletion_succeeded,
                ) {
                    root.restart_selected_wallet_sync_after_failed_deletion(window, cx);
                }
                match result {
                    Ok(Ok(deleted_wallet_ids)) => {
                        let mut participation_changed = false;
                        for wallet_id in &deleted_wallet_ids {
                            participation_changed |= remove_private_wallet_participants(
                                &mut root.ui_state.governance_participants,
                                wallet_id,
                            );
                        }
                        if participation_changed {
                            root.save_ui_state();
                            root.invalidate_governance_context();
                        }
                        root.manage_wallets.pending_delete_wallet_id = None;
                        root.manage_wallets.clear_hardware_delete_unlock_intent();
                        root.manage_wallets.error = None;
                        if deleting_selected_wallet {
                            root.view_session = None;
                            root.selected_wallet_id = None;
                            root.ui_state.last_wallet_id = None;
                            root.ui_state.last_wallet_kind = RememberedWalletKind::default();
                            root.save_ui_state();
                            #[cfg(feature = "hardware")]
                            {
                                root.active_hardware_profile = None;
                            }
                            root.sync_wallet_select(window, cx);
                        }
                        let _ = root.refresh_wallet_management_metadata(window, cx);
                    }
                    Ok(Err(WalletDeletionTaskError::Vault(error))) => {
                        root.handle_wallet_management_error(&error, cx);
                    }
                    Ok(Err(WalletDeletionTaskError::SyncShutdown(error))) => {
                        root.set_wallet_management_error(
                            format!("Failed to stop wallet sync before deletion: {error}"),
                            cx,
                        );
                    }
                    Ok(Err(WalletDeletionTaskError::Join(error))) => {
                        root.set_wallet_management_error(
                            format!("Wallet deletion task failed: {error}"),
                            cx,
                        );
                    }
                    Err(error) => {
                        root.set_wallet_management_error(
                            format!("Wallet deletion task failed: {error}"),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_inaccessible_hardware_delete_dialog(
        wallet_id: Arc<str>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(460.0));
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let cancel_root = root.clone();
            #[cfg(feature = "hardware")]
            let connect_root = root.clone();
            let hide_root = root.clone();
            let hide_wallet_id = Arc::clone(&wallet_id);
            #[cfg(feature = "hardware")]
            let connect_wallet_id = Arc::clone(&wallet_id);

            let actions = div()
                .w_full()
                .flex()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    app_button("wallet-delete-inaccessible-cancel", "Cancel")
                        .outline()
                        .on_click(move |_event, window, cx| {
                            window.close_dialog(cx);
                            cancel_root.update(cx, |root, _cx| {
                                root.manage_wallets.clear_hardware_delete_unlock_intent();
                            });
                        }),
                )
                .when(active, |actions| {
                    actions.child(
                        app_button("wallet-delete-inaccessible-hide", "Hide wallet")
                            .outline()
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                                let wallet_id = Arc::clone(&hide_wallet_id);
                                hide_root.update(cx, |root, cx| {
                                    root.manage_wallets.clear_hardware_delete_unlock_intent();
                                    root.set_wallet_visibility_from_dialog(
                                        wallet_id, false, window, cx,
                                    );
                                });
                            }),
                    )
                })
                .when(!active, |actions| {
                    actions.child(app_muted_text("This wallet is already hidden."))
                });

            #[cfg(feature = "hardware")]
            let actions = actions.child(
                app_button("wallet-delete-inaccessible-connect", "Connect wallet")
                    .primary()
                    .on_click(move |_event, window, cx| {
                        window.close_dialog(cx);
                        let wallet_id = Arc::clone(&connect_wallet_id);
                        connect_root.update(cx, |root, cx| {
                            root.manage_wallets
                                .begin_hardware_delete_unlock(Arc::clone(&wallet_id));
                            root.open_hardware_profile_unlock_dialog_for_wallet_deletion(
                                wallet_id, window, cx,
                            );
                        });
                    }),
            );

            dialog
                .w(dialog_width)
                .title(app_strong_text("Connect or hide hardware wallet"))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(app_muted_text(
                            "Permanent deletion needs the matching hardware wallet so legacy sync metadata can be attributed and removed safely. Hiding is reversible and keeps all wallet data.",
                        ))
                        .child(actions),
                )
        });
    }

    fn restart_selected_wallet_sync_after_failed_deletion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.sync_wallet_select(window, cx);
        self.reload_broadcaster_preferences(cx);
        self.reload_public_accounts(window, cx);
        self.schedule_public_balance_refresh(cx);
        self.schedule_inactive_public_balance_refresh(cx);
        self.ensure_chain_load(self.selected_chain, cx);
    }

    fn reorder_wallet_from_drop(
        &mut self,
        dragged_wallet_id: &str,
        drop_index: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let active_ids = active_wallet_management_rows(&self.wallet_metadata)
            .into_iter()
            .map(|metadata| Arc::<str>::from(metadata.wallet_uuid))
            .collect::<Vec<_>>();
        let Some(ordered_wallet_ids) =
            wallet_ids_after_drop(&active_ids, dragged_wallet_id, drop_index)
        else {
            return;
        };
        self.run_wallet_management_mutation(window, cx, move |store, view| {
            store
                .reorder_active_wallets_with_view_unlock(view, &ordered_wallet_ids)
                .map(|_| ())
        });
    }

    fn run_wallet_management_mutation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        mutate: impl FnOnce(&DesktopVaultStore, &ViewUnlock) -> Result<(), VaultError>,
    ) {
        if self.manage_wallets.deleting_wallet_id.is_some() {
            return;
        }
        let (store, view) = match self.wallet_management_context() {
            Ok(context) => context,
            Err(message) => {
                self.set_wallet_management_error(message, cx);
                return;
            }
        };
        match mutate(store.as_ref(), view.as_ref()) {
            Ok(()) => {
                self.manage_wallets.pending_delete_wallet_id = None;
                self.manage_wallets.error = None;
                self.refresh_wallet_management_metadata(window, cx);
            }
            Err(error) => self.handle_wallet_management_error(&error, cx),
        }
    }

    fn refresh_wallet_management_metadata(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let (store, view) = match self.wallet_management_context() {
            Ok(context) => context,
            Err(message) => {
                self.set_vault_error(message, cx);
                return false;
            }
        };
        match store.list_wallet_metadata_with_view_unlock(view.as_ref(), true) {
            Ok(metadata) => self.apply_wallet_management_metadata(&metadata, window, cx),
            Err(error) => {
                self.handle_vault_error(&error, cx);
                false
            }
        }
    }

    fn apply_wallet_management_metadata(
        &mut self,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let visible_metadata =
            visible_wallet_metadata(metadata, self.revealed_passphrase_context_id.as_deref());
        self.wallet_metadata.clone_from(&visible_metadata);
        self.wallet_options = wallet_options_from_metadata(visible_metadata.clone());
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let preserve_hidden_delete_session = wallet_management_preserves_hidden_delete_session(
            self.manage_wallets.hardware_delete_wallet_id.as_deref(),
            self.selected_wallet_id.as_deref(),
            self.view_session
                .as_deref()
                .map(DesktopViewSession::wallet_id),
        );
        let selection = if preserve_hidden_delete_session {
            WalletManagementSelection::KeepSelected
        } else {
            selected_wallet_after_metadata_refresh(
                self.selected_wallet_id.as_deref(),
                &self.wallet_options,
            )
        };
        match selection {
            WalletManagementSelection::KeepSelected => {
                self.sync_wallet_select(window, cx);
                cx.notify();
                true
            }
            WalletManagementSelection::SwitchTo(next_wallet_id) => {
                let next_requires_device = wallet_management_switch_requires_device(
                    next_wallet_id.as_ref(),
                    &self.wallet_options,
                );
                let (store, view) = match self.wallet_management_context() {
                    Ok(context) => context,
                    Err(message) => {
                        self.set_wallet_management_error(message, cx);
                        return false;
                    }
                };
                if next_requires_device {
                    self.enter_password_metadata_unlocked(
                        &visible_metadata,
                        view,
                        None,
                        None,
                        window,
                        cx,
                    );
                    return true;
                }
                match store
                    .load_view_session_with_view_unlock(view.as_ref(), next_wallet_id.as_ref())
                {
                    Ok(session) => {
                        self.install_view_session_after_management(session, metadata, window, cx);
                        true
                    }
                    Err(error) => {
                        self.handle_wallet_management_error(&error, cx);
                        false
                    }
                }
            }
            WalletManagementSelection::NoActiveWallet => {
                self.set_wallet_management_error(
                    "At least one active wallet is required. Show another wallet before hiding or deleting this one.",
                    cx,
                );
                false
            }
        }
    }

    fn wallet_management_context(
        &self,
    ) -> Result<(Arc<DesktopVaultStore>, Arc<ViewUnlock>), Arc<str>> {
        let store = self
            .vault_store
            .clone()
            .ok_or_else(|| Arc::from("Wallet vault storage is unavailable"))?;
        let view = self
            .vault_view_unlock
            .clone()
            .ok_or_else(|| Arc::from("Unlock the wallet vault before managing wallets"))?;
        Ok((store, view))
    }

    fn handle_wallet_management_error(&mut self, error: &VaultError, cx: &mut Context<'_, Self>) {
        tracing::warn!(
            error_kind = vault_error_kind(error),
            "desktop wallet management operation failed"
        );
        self.set_wallet_management_error(wallet_management_error_message(error), cx);
    }

    fn set_wallet_management_error(
        &mut self,
        message: impl Into<Arc<str>>,
        cx: &mut Context<'_, Self>,
    ) {
        self.manage_wallets.error = Some(message.into());
        cx.notify();
    }

    pub(super) fn render_manage_wallets_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Div {
        let active = active_wallet_management_rows(&self.wallet_metadata);
        let hidden = hidden_wallet_management_rows(&self.wallet_metadata);
        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_4()
            .children(self.manage_wallets.error.as_ref().map(|message| {
                Alert::error("wallet-management-error", message.to_string()).small()
            }))
            .child(self.render_active_wallets_section(root, &active))
            .child(self.render_hidden_wallets_section(root, &hidden))
    }

    fn render_active_wallets_section(
        &self,
        root: &Entity<Self>,
        active: &[WalletMetadataBundle],
    ) -> gpui::Div {
        let mut rows = Vec::with_capacity(active.len().saturating_mul(2).saturating_add(1));
        for (index, wallet) in active.iter().enumerate() {
            rows.push(Self::render_wallet_drop_zone(root.clone(), index).into_any_element());
            rows.push(
                self.render_wallet_management_row(root.clone(), wallet, true)
                    .into_any_element(),
            );
        }
        rows.push(Self::render_wallet_drop_zone(root.clone(), active.len()).into_any_element());

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Active"))
            .children(rows)
    }

    fn render_hidden_wallets_section(
        &self,
        root: &Entity<Self>,
        hidden: &[WalletMetadataBundle],
    ) -> gpui::Div {
        let content = if hidden.is_empty() {
            vec![
                app_muted_text("No hidden wallets.")
                    .py(px(8.0))
                    .into_any_element(),
            ]
        } else {
            hidden
                .iter()
                .map(|wallet| {
                    self.render_wallet_management_row(root.clone(), wallet, false)
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Hidden"))
            .children(content)
    }

    fn render_wallet_drop_zone(root: Entity<Self>, index: usize) -> impl IntoElement {
        div()
            .id(("wallet-management-drop-zone", index))
            .h(px(8.0))
            .rounded_sm()
            .drag_over::<WalletManagementDrag>(|this, _, _, _| {
                this.bg(rgb(theme::SELECTED_SURFACE))
            })
            .on_drop(cx_listener_for_drop(root, index))
    }

    fn render_wallet_management_row(
        &self,
        root: Entity<Self>,
        wallet: &WalletMetadataBundle,
        active: bool,
    ) -> impl IntoElement {
        let wallet_id: Arc<str> = Arc::from(wallet.wallet_uuid.clone());
        let label: Arc<str> = Arc::from(wallet.label.clone());
        let is_current = self.selected_wallet_id.as_deref() == Some(wallet.wallet_uuid.as_str());
        let is_editing =
            self.manage_wallets.editing_wallet_id.as_deref() == Some(wallet.wallet_uuid.as_str());
        let confirming_delete = wallet.source.is_hardware_derived()
            && self.manage_wallets.pending_delete_wallet_id.as_deref()
                == Some(wallet.wallet_uuid.as_str());
        let drag = WalletManagementDrag {
            wallet_id: Arc::clone(&wallet_id),
            label: Arc::clone(&label),
        };
        let affordance = if active {
            div()
                .id(SharedString::from(format!(
                    "wallet-management-drag-{}",
                    wallet.wallet_uuid
                )))
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_move()
                .text_color(rgb(theme::TEXT_MUTED))
                .tooltip(|window, cx| {
                    Tooltip::new("Drag to reorder active wallets").build(window, cx)
                })
                .child(Icon::new(IconName::ChevronsUpDown).small())
                .on_drag(drag, |drag, position, _window, cx| {
                    let label = Arc::clone(&drag.label);
                    cx.new(|_| WalletManagementDragPreview { label, position })
                })
                .into_any_element()
        } else {
            div()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(Icon::new(IconName::EyeOff).small())
                .into_any_element()
        };

        let label_content = if is_editing {
            self.render_wallet_label_editor(root.clone())
                .into_any_element()
        } else {
            wallet_label_content(wallet, is_current).into_any_element()
        };

        div()
            .id(SharedString::from(format!(
                "wallet-management-row-{}",
                wallet.wallet_uuid
            )))
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px(px(10.0))
            .py(px(8.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .child(affordance)
            .child(div().flex_1().min_w(px(0.0)).child(label_content))
            .child(Self::render_wallet_row_actions(
                root,
                &wallet_id,
                &label,
                wallet.source,
                active,
                confirming_delete,
                self.manage_wallets.deleting_wallet_id.is_some(),
            ))
    }

    fn render_wallet_label_editor(&self, root: Entity<Self>) -> gpui::Div {
        let save_root = root.clone();
        let cancel_root = root;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(app_input(&self.manage_wallet_label_input)),
            )
            .child(
                app_button("wallet-management-save-label", "Save")
                    .primary()
                    .xsmall()
                    .on_click(move |_event, window, cx| {
                        save_root.update(cx, |root, cx| {
                            root.save_wallet_label_edit(window, cx);
                        });
                    }),
            )
            .child(
                app_button("wallet-management-cancel-label", "Cancel")
                    .outline()
                    .xsmall()
                    .on_click(move |_event, _window, cx| {
                        cancel_root.update(cx, |root, cx| {
                            root.cancel_wallet_label_edit(cx);
                        });
                    }),
            )
    }

    fn render_wallet_row_actions(
        root: Entity<Self>,
        wallet_id: &Arc<str>,
        label: &Arc<str>,
        source: WalletSource,
        active: bool,
        confirming_delete: bool,
        deletion_in_progress: bool,
    ) -> gpui::Div {
        let edit_root = root.clone();
        #[cfg(feature = "hardware")]
        let unlock_root = root.clone();
        let visibility_root = root.clone();
        let delete_root = root.clone();
        let cancel_delete_root = root;
        let edit_wallet_id = Arc::clone(wallet_id);
        #[cfg(feature = "hardware")]
        let unlock_wallet_id = Arc::clone(wallet_id);
        let edit_label = Arc::clone(label);
        let visibility_wallet_id = Arc::clone(wallet_id);
        let delete_wallet_id = Arc::clone(wallet_id);
        let cancel_wallet_id = Arc::clone(wallet_id);
        let mut actions = div().flex().items_center().gap_2();

        if active {
            actions = actions
                .child(
                    wallet_management_icon_button(
                        SharedString::from(format!("wallet-management-rename-{wallet_id}")),
                        Icon::new(RailgunActionIcon::Pencil),
                        "Rename wallet",
                    )
                    .on_click(move |_event, window, cx| {
                        let wallet_id = Arc::clone(&edit_wallet_id);
                        let label = Arc::clone(&edit_label);
                        edit_root.update(cx, |root, cx| {
                            root.begin_wallet_label_edit(wallet_id, label.as_ref(), window, cx);
                        });
                    }),
                )
                .when(source.is_hardware_derived(), |actions| {
                    #[cfg(feature = "hardware")]
                    {
                        actions.child(
                            wallet_management_icon_button(
                                SharedString::from(format!(
                                    "wallet-management-unlock-profile-{wallet_id}"
                                )),
                                IconName::Eye,
                                "Connect hardware wallet",
                            )
                            .on_click(move |_event, window, cx| {
                                let wallet_id = Arc::clone(&unlock_wallet_id);
                                unlock_root.update(cx, |root, cx| {
                                    root.open_hardware_profile_unlock_dialog_for_wallet(
                                        wallet_id, window, cx,
                                    );
                                });
                            }),
                        )
                    }
                    #[cfg(not(feature = "hardware"))]
                    {
                        actions
                    }
                })
                .child(
                    wallet_management_icon_button(
                        SharedString::from(format!("wallet-management-hide-{wallet_id}")),
                        IconName::EyeOff,
                        "Hide wallet",
                    )
                    .on_click(move |_event, window, cx| {
                        let wallet_id = Arc::clone(&visibility_wallet_id);
                        visibility_root.update(cx, |root, cx| {
                            root.set_wallet_visibility_from_dialog(wallet_id, false, window, cx);
                        });
                    }),
                );
        } else {
            actions = actions.child(
                wallet_management_icon_button(
                    SharedString::from(format!("wallet-management-show-{wallet_id}")),
                    IconName::Eye,
                    "Show wallet",
                )
                .on_click(move |_event, window, cx| {
                    let wallet_id = Arc::clone(&visibility_wallet_id);
                    visibility_root.update(cx, |root, cx| {
                        root.set_wallet_visibility_from_dialog(wallet_id, true, window, cx);
                    });
                }),
            );
        }

        actions = actions.child(
            wallet_management_icon_button(
                SharedString::from(format!("wallet-management-delete-{wallet_id}")),
                Icon::new(RailgunActionIcon::Trash2),
                if confirming_delete {
                    "Confirm delete wallet"
                } else {
                    "Delete wallet"
                },
            )
            .danger()
            .disabled(deletion_in_progress)
            .on_click(move |_event, window, cx| {
                let wallet_id = Arc::clone(&delete_wallet_id);
                delete_root.update(cx, |root, cx| {
                    root.delete_wallet_from_dialog(wallet_id, false, window, cx);
                });
            }),
        );

        if confirming_delete {
            actions = actions.child(
                app_button(
                    SharedString::from(format!("wallet-management-cancel-delete-{wallet_id}")),
                    "Cancel",
                )
                .outline()
                .xsmall()
                .disabled(deletion_in_progress)
                .on_click(move |_event, _window, cx| {
                    let wallet_id = Arc::clone(&cancel_wallet_id);
                    cancel_delete_root.update(cx, |root, cx| {
                        if root.manage_wallets.pending_delete_wallet_id.as_deref()
                            == Some(wallet_id.as_ref())
                        {
                            root.manage_wallets.pending_delete_wallet_id = None;
                            root.manage_wallets.clear_hardware_delete_unlock_intent();
                            root.manage_wallets.error = None;
                            cx.notify();
                        }
                    });
                }),
            );
        }

        actions
    }
}

fn cx_listener_for_drop(
    root: Entity<WalletRoot>,
    index: usize,
) -> impl Fn(&WalletManagementDrag, &mut Window, &mut App) + 'static {
    move |drag, window, cx| {
        let dragged_wallet_id = drag.wallet_id.to_string();
        root.update(cx, |root, cx| {
            root.reorder_wallet_from_drop(&dragged_wallet_id, index, window, cx);
        });
    }
}

fn section_title(label: &'static str) -> gpui::Div {
    app_strong_text(label).font_weight(FontWeight::SEMIBOLD)
}

fn wallet_management_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    Button::new(id).icon(icon).ghost().small().tooltip(tooltip)
}

fn wallet_label_content(wallet: &WalletMetadataBundle, current: bool) -> gpui::Div {
    let mut content = div().flex().flex_col().gap_1().min_w(px(0.0)).child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(app_strong_text(wallet.label.clone()).truncate())
            .children(current.then(|| {
                app_muted_text("Current")
                    .text_color(rgb(theme::PRIMARY))
                    .font_weight(FontWeight::SEMIBOLD)
            })),
    );
    content = content.child(
        app_muted_text(wallet_source_label(wallet))
            .text_size(px(11.0))
            .truncate(),
    );
    content
}

pub(super) fn wallet_source_label(wallet: &WalletMetadataBundle) -> String {
    match wallet.source {
        WalletSource::Generated => "Generated wallet".to_owned(),
        WalletSource::Imported => "Imported wallet".to_owned(),
        WalletSource::LedgerDerived => hardware_wallet_source_label("Ledger", wallet),
        WalletSource::TrezorDerived => hardware_wallet_source_label("Trezor", wallet),
    }
}

fn hardware_wallet_source_label(device_label: &str, wallet: &WalletMetadataBundle) -> String {
    wallet.hardware_derivation_descriptor().map_or_else(
        || format!("{device_label}-derived wallet"),
        |descriptor| {
            format!(
                "{device_label}-derived wallet - account {}",
                descriptor.account_index
            )
        },
    )
}

fn wallet_management_error_message(error: &VaultError) -> Arc<str> {
    match error {
        VaultError::InvalidWalletLabel => Arc::from("Enter a wallet label before saving."),
        VaultError::DuplicateWalletLabel => Arc::from("A wallet with that label already exists."),
        VaultError::InvalidWalletOrder => Arc::from("Wallet order changed. Try dragging again."),
        VaultError::LastActiveWallet => Arc::from(
            "At least one active wallet is required. Show another wallet before hiding or deleting this one.",
        ),
        VaultError::WalletNotFound => Arc::from("Wallet not found. Refresh and try again."),
        VaultError::StandardWalletHasPassphraseChildren => Arc::from(
            "This standard wallet has passphrase wallets. Use the confirmed profile deletion action to remove the whole profile.",
        ),
        VaultError::HardwareWalletViewRequiresDevice => {
            Arc::from("Unlock and select the matching hardware wallet before deleting it.")
        }
        VaultError::WalletChainMetadataUnavailable => Arc::from(
            "Legacy wallet sync metadata is malformed and cannot be safely assigned to a wallet. Deletion stopped before any wallet data was changed.",
        ),
        _ => Arc::from(format!("Wallet management failed: {error}")),
    }
}

pub(super) const fn software_wallet_delete_confirmation_copy() -> &'static str {
    "This will permanently delete this wallet and its Public accounts and local private sync data. Any passphrase wallets associated with this wallet will also be deleted."
}
