//! Native confirmation and lifecycle identity for one paused browser connect.
use crate::root::WalletRoot;
use gpui::{Context, FocusHandle, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::WindowExt;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use ui::controls::{app_button, app_muted_text, app_strong_text};
use wallet_ops::gateway::{
    GatewayWalletState, GatewayWalletSwitchRequest, GatewayWalletSwitchTransition,
};

pub(super) struct GatewaySwitchDialog {
    request: Arc<GatewayWalletSwitchRequest>,
    focus: FocusHandle,
    lease: Weak<()>,
    submitting: bool,
}

pub(super) struct GatewaySwitchContinuation {
    request: Arc<GatewayWalletSwitchRequest>,
    phase: SwitchPhase,
}

#[derive(Clone, Copy)]
enum SwitchPhase {
    Dispatching,
    Loading {
        operation: u64,
    },
    Installing {
        operation: u64,
        wallet_generation: u64,
    },
    Installed {
        operation: u64,
        wallet_generation: u64,
    },
}

impl SwitchPhase {
    #[cfg(any(feature = "hardware", test))]
    fn advance_hardware_loading(
        &mut self,
        original_target: &str,
        target: &str,
        current_operation: u64,
    ) {
        if original_target == target
            && matches!(*self, Self::Loading { operation } if operation == current_operation)
        {
            *self = Self::Loading {
                operation: current_operation.wrapping_add(1),
            };
        }
    }
}

impl WalletRoot {
    fn gateway_switch_source_is_current(&self, request: &GatewayWalletSwitchRequest) -> bool {
        self.gateway.handle.is_some()
            && self.manage_wallets.deleting_wallet_id.is_none()
            && self.gateway.desktop_state.as_ref().is_some_and(|state| {
                let (wallet, generation, _) = &*state.borrow();
                request.source_is_current(wallet, *generation)
            })
    }

    pub(super) fn reconcile_gateway_wallet_switches(
        &mut self,
        requests: &[Arc<GatewayWalletSwitchRequest>],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(dialog) = &self.gateway.switch_dialog {
            let current = requests
                .iter()
                .any(|request| Arc::ptr_eq(request, &dialog.request))
                && self.gateway_switch_source_is_current(&dialog.request);
            let gone = dialog.lease.upgrade().is_none();
            if gone || !current {
                let own_dialog = !gone && dialog.focus.contains_focused(window, cx);
                let request = dialog.request.clone();
                if gone || own_dialog {
                    self.gateway.switch_dialog = None;
                    if own_dialog {
                        window.close_dialog(cx);
                    }
                    self.reject_gateway_wallet_switch(&request);
                }
            }
        }
        if self
            .gateway
            .wallet_switch
            .borrow()
            .as_ref()
            .is_some_and(|continuation| continuation.request.control.ensure_current().is_err())
        {
            self.gateway.wallet_switch.borrow_mut().take();
            self.publish_gateway_desktop_state();
        }
        if self.gateway.switch_dialog.is_some()
            || self.gateway.wallet_switch.borrow().is_some()
            || window.has_active_dialog(cx)
        {
            return;
        }
        let Some(request) = requests
            .iter()
            .find(|request| {
                request.awaiting_confirmation() && self.gateway_switch_source_is_current(request)
            })
            .cloned()
        else {
            return;
        };
        let label = self
            .wallet_metadata
            .iter()
            .find(|wallet| wallet.wallet_uuid == request.target_wallet_uuid)
            .map_or_else(
                || "the wallet that owns this account".to_owned(),
                |wallet| wallet.label.clone(),
            );
        let root = cx.entity();
        let lease = Rc::new(());
        let weak_lease = Rc::downgrade(&lease);
        let displayed = request.clone();
        let width = (window.viewport_size().width * 0.92).min(px(520.0));
        let max_height = window.viewport_size().height * 0.8;
        window.open_dialog(cx, move |dialog, _, _| {
            let _lease = Rc::clone(&lease);
            let close_root = root.clone();
            let close_request = displayed.clone();
            let approve_root = root.clone();
            let approve_request = displayed.clone();
            let reject_root = root.clone();
            let reject_request = displayed.clone();
            dialog.w(width).max_h(max_height).title(app_strong_text("Switch wallet for this website?"))
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| close_root.update(cx, |root, _| {
                    if root.gateway.switch_dialog.as_ref().is_some_and(|dialog| Arc::ptr_eq(&dialog.request, &close_request)) {
                        root.gateway.switch_dialog = None;
                        root.reject_gateway_wallet_switch(&close_request);
                    }
                }))
                .child(div().flex().flex_col().gap_3()
                    .child(app_muted_text(displayed.url.clone()))
                    .child(app_muted_text(format!("This website's saved account belongs to a different wallet. Switch to {label} and resync it?")))
                    .child(app_muted_text("This keeps the saved account permission. You can instead choose an account from the active wallet in the extension.")))
                .footer(gpui_component::dialog::DialogFooter::new().children([
                    app_button("gateway-switch-reject", "Cancel").on_click(move |_, window, cx| reject_root.update(cx, |root, cx| {
                        root.close_gateway_switch_dialog(&reject_request, window, cx);
                        root.reject_gateway_wallet_switch(&reject_request);
                    })).into_any_element(),
                    app_button("gateway-switch-confirm", "Switch and resync").on_click(move |_, window, cx| approve_root.update(cx, |root, cx| root.confirm_gateway_wallet_switch(approve_request.clone(), window, cx))).into_any_element(),
                ]))
        });
        if let Some(focus) = window.focused(cx) {
            self.gateway.switch_dialog = Some(GatewaySwitchDialog {
                request,
                focus,
                lease: weak_lease,
                submitting: false,
            });
        }
    }

    fn close_gateway_switch_dialog(
        &mut self,
        request: &Arc<GatewayWalletSwitchRequest>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.switch_dialog.as_ref().is_some_and(|dialog| {
            Arc::ptr_eq(&dialog.request, request) && dialog.focus.contains_focused(window, cx)
        }) {
            self.gateway.switch_dialog = None;
            window.close_dialog(cx);
        }
    }

    fn reject_gateway_wallet_switch(&self, request: &GatewayWalletSwitchRequest) {
        request
            .control
            .invalidate(&wallet_ops::RpcBrokerError::OriginRejected);
        if let Some(handle) = self.gateway.handle.clone() {
            let id = request.id.clone();
            self.runtime.spawn(async move {
                handle.reject_wallet_switch(id).await;
            });
        }
    }

    fn confirm_gateway_wallet_switch(
        &mut self,
        request: Arc<GatewayWalletSwitchRequest>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        if !self.gateway_switch_source_is_current(&request)
            || !request.awaiting_confirmation()
            || !self
                .gateway
                .switch_dialog
                .as_ref()
                .is_some_and(|dialog| Arc::ptr_eq(&dialog.request, &request) && !dialog.submitting)
        {
            return;
        }
        self.gateway
            .switch_dialog
            .as_mut()
            .expect("current dialog")
            .submitting = true;
        let handle = self.gateway.handle.clone().expect("current gateway");
        let id = request.id.clone();
        let join = self
            .runtime
            .spawn(async move { handle.begin_wallet_switch(id).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !matches!(result, Ok(Ok(ref admitted)) if Arc::ptr_eq(admitted, &request))
                    || !root.gateway_switch_source_is_current(&request)
                    || !root.gateway.switch_dialog.as_ref().is_some_and(|dialog| {
                        Arc::ptr_eq(&dialog.request, &request)
                            && dialog.submitting
                            && dialog.focus.contains_focused(window, cx)
                    })
                {
                    root.close_gateway_switch_dialog(&request, window, cx);
                    root.reject_gateway_wallet_switch(&request);
                    return;
                }
                root.close_gateway_switch_dialog(&request, window, cx);
                *root.gateway.wallet_switch.borrow_mut() = Some(GatewaySwitchContinuation {
                    request: request.clone(),
                    phase: SwitchPhase::Dispatching,
                });
                root.publish_gateway_desktop_state();
                // Existing software/hardware admission, cleanup, generation fencing and resync.
                root.select_wallet(&request.target_wallet_uuid, window, cx);
                if let Some(continuation) = root.gateway.wallet_switch.borrow_mut().as_mut() {
                    continuation.phase = SwitchPhase::Loading {
                        operation: root.wallet_switch_generation,
                    };
                }
                root.publish_gateway_desktop_state();
            });
        })
        .detach();
    }

    /// The current hardware unlock callback alone may transfer its exact loading operation.
    #[cfg(feature = "hardware")]
    pub(in crate::root) fn advance_gateway_hardware_wallet_switch(
        &self,
        profile: &wallet_ops::vault::HardwareProfileMetadata,
        metadata: &[wallet_ops::vault::WalletMetadataBundle],
    ) {
        use crate::root::vault::{HardwareProfileUnlockPurpose, hardware_account_picker_rows};
        let state = &self.hardware_profile_unlock;
        let Some(target) = state.target_wallet_id.as_deref() else {
            return;
        };
        if state.purpose != HardwareProfileUnlockPurpose::Open {
            return;
        }
        let (accounts, _) = hardware_account_picker_rows(
            metadata,
            &profile.profile_id,
            self.selected_wallet_id.as_deref(),
            state.purpose,
            Some(target),
        );
        if !accounts
            .iter()
            .any(|row| row.wallet_id.as_ref() == target && row.supported)
        {
            return;
        }
        let mut network = GatewayWalletState::default();
        network.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
        let mut continuation = self.gateway.wallet_switch.borrow_mut();
        let Some(active) = continuation.as_mut() else {
            return;
        };
        if self.gateway.handle.is_none()
            || self.manage_wallets.deleting_wallet_id.is_some()
            || active.request.control.ensure_current().is_err()
            || !active.request.same_network(&network)
            || !self
                .gateway
                .desktop_state
                .as_ref()
                .is_some_and(|state| active.request.source_wallet_is_current(&state.borrow().0))
        {
            return;
        }
        active.phase.advance_hardware_loading(
            &active.request.target_wallet_uuid,
            target,
            self.wallet_switch_generation,
        );
    }

    /// Called before installation invalidates the previous view and awaits its cleanup.
    pub(in crate::root) fn begin_gateway_wallet_installation(&self, target: &str) {
        let mut continuation = self.gateway.wallet_switch.borrow_mut();
        let admitted = continuation.as_ref().is_some_and(|continuation| {
            continuation.request.target_wallet_uuid == target
                && continuation.request.control.ensure_current().is_ok()
                && matches!(continuation.phase, SwitchPhase::Loading { operation } if operation == self.wallet_switch_generation)
        });
        if admitted {
            continuation.as_mut().expect("admitted switch").phase = SwitchPhase::Installing {
                operation: self.wallet_switch_generation.wrapping_add(1),
                wallet_generation: self.active_wallet_generation.wrapping_add(1),
            };
        } else {
            *continuation = None;
        }
    }

    pub(in crate::root) fn finish_gateway_wallet_installation(&self) {
        let mut continuation = self.gateway.wallet_switch.borrow_mut();
        if let Some(continuation) = continuation.as_mut()
            && let SwitchPhase::Installing {
                operation,
                wallet_generation,
            } = continuation.phase
            && operation == self.wallet_switch_generation
            && wallet_generation == self.active_wallet_generation
            && self
                .view_session
                .as_ref()
                .is_some_and(|view| view.wallet_id() == continuation.request.target_wallet_uuid)
        {
            continuation.phase = SwitchPhase::Installed {
                operation,
                wallet_generation,
            };
        } else {
            *continuation = None;
        }
    }

    pub(super) fn gateway_wallet_switch_transition(
        &self,
        snapshot: &GatewayWalletState,
    ) -> Option<GatewayWalletSwitchTransition> {
        let mut continuation = self.gateway.wallet_switch.borrow_mut();
        let active = continuation.as_ref()?;
        let mut network = GatewayWalletState::default();
        network.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
        let current = active.request.control.ensure_current().is_ok()
            && active.request.same_network(&network)
            && self.manage_wallets.deleting_wallet_id.is_none()
            && match active.phase {
                SwitchPhase::Dispatching => active.request.source_wallet_is_current(snapshot),
                SwitchPhase::Loading { operation } => {
                    operation == self.wallet_switch_generation
                        && active.request.source_wallet_is_current(snapshot)
                }
                // Synchronous installation publishes while incrementing both generations and clearing the source view.
                SwitchPhase::Installing {
                    operation,
                    wallet_generation,
                } => {
                    (self.wallet_switch_generation == operation
                        || self.wallet_switch_generation == operation.wrapping_sub(1))
                        && (self.active_wallet_generation == wallet_generation
                            || self.active_wallet_generation == wallet_generation.wrapping_sub(1))
                }
                SwitchPhase::Installed {
                    operation,
                    wallet_generation,
                } => {
                    operation == self.wallet_switch_generation
                        && wallet_generation == self.active_wallet_generation
                        && snapshot.view.as_ref().is_some_and(|view| {
                            view.wallet_id() == active.request.target_wallet_uuid
                        })
                }
            };
        if !current {
            *continuation = None;
            return None;
        }
        Some(GatewayWalletSwitchTransition {
            request_id: active.request.id.clone(),
            installed: matches!(active.phase, SwitchPhase::Installed { .. }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SwitchPhase;

    #[test]
    fn hardware_loading_transfers_only_the_original_target_and_exact_operation() {
        let mut phase = SwitchPhase::Loading { operation: 7 };
        phase.advance_hardware_loading("target", "other", 7);
        assert!(matches!(phase, SwitchPhase::Loading { operation: 7 }));
        phase.advance_hardware_loading("target", "target", 8);
        assert!(matches!(phase, SwitchPhase::Loading { operation: 7 }));
        phase.advance_hardware_loading("target", "target", 7);
        assert!(matches!(phase, SwitchPhase::Loading { operation: 8 }));
        phase.advance_hardware_loading("target", "target", 7);
        assert!(matches!(phase, SwitchPhase::Loading { operation: 8 }));
    }
}
