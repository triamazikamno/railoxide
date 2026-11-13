use std::cell::Cell;
use std::rc::Weak;
use std::sync::Arc;

use alloy::primitives::U256;
use gpui::{
    AnyElement, App, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, Window, div, prelude::FluentBuilder as _, rgb,
};
use gpui_component::{
    Disableable, Sizable, WindowExt,
    alert::Alert,
    button::ButtonVariants,
    input::{InputEvent, InputState},
    select::{SearchableVec, SelectEvent},
    spinner::Spinner,
};
use ui::controls::{app_button, app_muted_shimmer_text, app_muted_text, app_notice};
#[cfg(feature = "hardware")]
use ui::controls::{app_masked_input, app_strong_text};
use ui::theme;
use wallet_ops::DesktopPrivateSpendAuthorization;
use wallet_ops::vault::VaultError;
use zeroize::Zeroizing;

use super::super::device_auth::{
    DEVICE_AUTH_REASON_SPEND, DeviceAuthMethod, DeviceAuthPassword, DeviceAuthPrompt,
    device_auth_buttons, masked_input_with_device_auth,
};
use super::super::{WalletRoot, new_masked_input};
use super::hardware::HardwareSpendAuthorizationCompletion;
#[cfg(feature = "hardware")]
use super::hardware::hardware_spend_authorization_error_message;
use super::intent::SpendAuthorizationIntent;
use super::summary::SpendAuthorizationSummary;
use super::summary_view::{
    new_spend_authorization_lifetime_select, render_spend_authorization_details,
    render_spend_authorization_lifetime_row, render_spend_authorization_payload,
    render_spend_authorization_steps, render_spend_authorization_summary,
};
use super::{
    SPEND_AUTHORIZATION_DETAILS_TOGGLE, SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
    SpendAuthorizationLifetime, SpendAuthorizationLifetimeSelect, SpendAuthorizationScope,
};

const SPEND_AUTHORIZATION_SESSION_WARNING: &str = "Spending remains authorized for the selected lifetime without re-entering the password. Only use this on a trusted device.";

pub(super) struct SpendAuthorizationDialogContent {
    root: Entity<WalletRoot>,
    intent: SpendAuthorizationIntent,
    summary: SpendAuthorizationSummary,
    pub(super) password_input: Entity<InputState>,
    pub(super) lifetime: SpendAuthorizationLifetime,
    pub(super) lifetime_select: Entity<SpendAuthorizationLifetimeSelect>,
    payload_open: bool,
    row_group_open: bool,
    details_open: bool,
    pub(super) error: Option<Arc<str>>,
    pub(super) pending: bool,
    cancelled: bool,
    pub(super) review_authorization:
        Option<(SpendAuthorizationScope, DesktopPrivateSpendAuthorization)>,
    review_focus: gpui::FocusHandle,
    pub(super) device_auth: Option<DeviceAuthPrompt>,
    pub(super) device_auth_pending: bool,
    lease: Weak<Cell<bool>>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct HardwareGasPaymentReview {
    pub(super) form: gpui::EntityId,
    pub(super) recipient: String,
    pub(super) amount: U256,
    pub(super) payer: Option<String>,
    pub(super) funding: super::super::private_action::SelfBroadcastFundingMode,
    pub(super) gas_fee: wallet_ops::SelfBroadcastGasFeeSelection,
    pub(super) incentive: wallet_ops::SponsoredIncentive,
    pub(super) fee_mode: wallet_ops::FeeHandlingMode,
    pub(super) unwrap: bool,
    pub(super) top_up: Option<wallet_ops::DesktopNativeTopUpPlan>,
}

#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
pub(super) struct HardwareSpendAuthorizationDialogContent {
    root: Entity<WalletRoot>,
    completion: HardwareSpendAuthorizationCompletion,
    gas_review: Option<HardwareGasPaymentReview>,
    summary: SpendAuthorizationSummary,
    device_label: &'static str,
    pending: bool,
    cancelled: bool,
    completed: bool,
    payload_open: bool,
    row_group_open: bool,
    details_open: bool,
    error: Option<Arc<str>>,
}

impl HardwareSpendAuthorizationDialogContent {
    #[allow(clippy::missing_const_for_fn)]
    pub(super) fn new(
        root: Entity<WalletRoot>,
        completion: HardwareSpendAuthorizationCompletion,
        gas_review: Option<HardwareGasPaymentReview>,
        summary: SpendAuthorizationSummary,
        device_label: &'static str,
    ) -> Self {
        Self {
            root,
            completion,
            gas_review,
            summary,
            device_label,
            pending: false,
            cancelled: false,
            completed: false,
            payload_open: false,
            row_group_open: false,
            details_open: false,
            error: None,
        }
    }

    pub(super) fn cancel(&mut self, cx: &mut Context<'_, Self>) {
        if self.completed {
            return;
        }
        self.cancelled = true;
        if let Some(intent) = self.completion.private_intent() {
            self.root.update(cx, |root, cx| {
                if let Some(execution) = intent.gateway_execution() {
                    execution.cancel_authorization();
                    root.release_gateway_private_form(execution, cx);
                }
                root.cancel_spend_authorization(&intent, cx);
            });
        }
        cx.notify();
    }

    fn toggle_payload(&mut self, cx: &mut Context<'_, Self>) {
        self.payload_open = !self.payload_open;
        cx.notify();
    }

    fn toggle_row_group(&mut self, cx: &mut Context<'_, Self>) {
        self.row_group_open = !self.row_group_open;
        cx.notify();
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    /// The dialog's footer, which stays in view while the content scrolls.
    pub(super) fn render_footer(&self, dialog: &Entity<Self>) -> gpui::Div {
        let dialog = dialog.clone();
        let pending = self.pending;
        let submit_label = if pending || self.error.is_none() {
            format!("Approve on {}", self.device_label)
        } else {
            "Try again".to_owned()
        };
        div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-hardware-spend-auth-cancel", "Cancel")
                    .flex_none()
                    .disabled(pending)
                    .on_click(move |_event, window, cx| {
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button("wallet-hardware-spend-auth-submit", submit_label)
                    .primary()
                    .flex_none()
                    .disabled(pending)
                    .on_click(move |_event, window, cx| {
                        dialog.update(cx, |dialog, cx| dialog.start(window, cx));
                    }),
            )
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) fn start(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending {
            return;
        }
        self.pending = true;
        self.cancelled = false;
        self.error = None;
        cx.notify();

        #[cfg(not(feature = "hardware"))]
        {
            let _ = window;
            self.pending = false;
            self.error = Some(Arc::from(
                "Hardware wallet support is not enabled in this build. Rebuild the wallet with the hardware feature to authorize hardware-derived spends.",
            ));
            cx.notify();
        }

        #[cfg(feature = "hardware")]
        {
            let root = self.root.clone();
            let completion = self.completion.clone();
            let gas_review = self.gas_review.clone();
            if root.update(cx, |root, cx| {
                root.hardware_gas_payment_review(&completion, cx)
            }) != gas_review
            {
                self.pending = false;
                self.error =
                    Some("The action changed. Close this dialog and review it again.".into());
                cx.notify();
                return;
            }
            let approved_view = root.read(cx).view_session.clone();
            let approved_generation = root.read(cx).active_wallet_generation;
            let task = root.update(cx, |root, cx| {
                root.start_hardware_spend_authorization_task(&completion, window, cx)
            });
            match task {
                Ok(join) => {
                    cx.spawn_in(window, async move |this, cx| {
                        let result = join.await;
                        let _ = this.update_in(cx, |dialog, window, cx| {
                            if dialog.cancelled {
                                return;
                            }
                            dialog.pending = false;
                            match result {
                                Ok(Ok((authorization, destination, hardware_session))) => {
                                    let root = dialog.root.clone();
                                    if root.read(cx).active_wallet_generation != approved_generation
                                        || !approved_view.as_ref().zip(root.read(cx).view_session.as_ref())
                                            .is_some_and(|(approved, current)| approved.is_same_wallet_session(current))
                                    {
                                        dialog.error = Some("The wallet session changed. Close this dialog and authorize the action again.".into());
                                        cx.notify();
                                        return;
                                    }
                                    if root.update(cx, |root, cx| root.hardware_gas_payment_review(&completion, cx)) != gas_review {
                                        dialog.error = Some("The action changed while awaiting the device. Review it again.".into());
                                        cx.notify();
                                        return;
                                    }
                                    if let Some(intent) = completion.private_intent()
                                        && !intent.approve_gateway_review(root.read(cx))
                                    {
                                        let message = root.update(cx, |root, cx| root.report_stale_private_fee_review(&intent, cx));
                                        dialog.error = Some(message.map_or_else(|| "The action changed while awaiting the device. Review it again.".into(), Into::into));
                                        cx.notify();
                                        return;
                                    }
                                    dialog.completed = true;
                                    window.close_dialog(cx);
                                    root.update(cx, |root, cx| {
                                        root.refresh_active_hardware_profile_session(
                                            hardware_session,
                                            cx,
                                        );
                                        match completion {
                                            HardwareSpendAuthorizationCompletion::Continue(intent) | HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer { intent, .. } => {
                                                root.continue_authorized_spend_with_destination(
                                                    intent,
                                                    authorization,
                                                    destination,
                                                    window,
                                                    cx,
                                                );
                                            }
                                             HardwareSpendAuthorizationCompletion::PrivateSendSelfBroadcast {
                                                 key,
                                                 vault_password,
                                                 authorization_limit,
                                                 execution,
                                             } => {
                                                root.generate_send_calldata_authorized_with_gas_password(
                                                    key,
                                                     authorization,
                                                     Some(vault_password),
                                                     authorization_limit,
                                                     None,
                                                     window,
                                                    cx,
                                                );
                                                if let Some(execution) = execution {
                                                    root.reject_gateway_private_authorization(&execution, cx);
                                                }
                                            }
                                             HardwareSpendAuthorizationCompletion::PrivateUnshieldSelfBroadcast {
                                                 key,
                                                 vault_password,
                                                 authorization_limit,
                                                 execution,
                                             } => {
                                                root.generate_unshield_calldata_authorized_with_gas_password(
                                                    key,
                                                     authorization,
                                                     Some(vault_password),
                                                     authorization_limit,
                                                     None,
                                                     window,
                                                    cx,
                                                );
                                                if let Some(execution) = execution {
                                                    root.reject_gateway_private_authorization(&execution, cx);
                                                }
                                            }
                                            HardwareSpendAuthorizationCompletion::BlockedShieldRefund {
                                                utxo_id,
                                                vault_password,
                                            } => {
                                                root.submit_blocked_shield_refund_authorized(
                                                    utxo_id,
                                                    authorization,
                                                    Some(vault_password),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }
                                    });
                                }
                                Ok(Err(error)) => {
                                    let message = hardware_spend_authorization_error_message(&error);
                                    let root = dialog.root.clone();
                                    root.update(cx, |root, cx| {
                                        root.discard_active_trezor_session_if_stale(&message, cx);
                                    });
                                    dialog.error = Some(Arc::from(message));
                                    cx.notify();
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "desktop hardware spend authorization task failed");
                                    dialog.error = Some(Arc::from(
                                        "Hardware spend authorization failed. See logs for non-sensitive diagnostics.",
                                    ));
                                    cx.notify();
                                }
                            }
                        });
                    })
                    .detach();
                }
                Err(message) => {
                    self.pending = false;
                    self.error = Some(message);
                    cx.notify();
                }
            }
        }
    }
}

impl SpendAuthorizationDialogContent {
    pub(super) fn new(
        root: Entity<WalletRoot>,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        initial_lifetime: SpendAuthorizationLifetime,
        lease: Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let password_input = new_masked_input(window, cx, "Vault password");
        cx.subscribe_in(
            &password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        let lifetime_select = new_spend_authorization_lifetime_select(initial_lifetime, window, cx);
        cx.subscribe(
            &lifetime_select,
            |this, _select, event: &SelectEvent<SearchableVec<SpendAuthorizationLifetime>>, cx| {
                if let SelectEvent::Confirm(Some(lifetime)) = event {
                    this.set_lifetime(*lifetime, cx);
                }
            },
        )
        .detach();
        Self {
            root,
            intent,
            summary,
            password_input,
            lifetime: initial_lifetime,
            lifetime_select,
            payload_open: false,
            row_group_open: false,
            details_open: false,
            error: None,
            pending: false,
            cancelled: false,
            review_authorization: None,
            review_focus: cx.focus_handle(),
            device_auth: None,
            device_auth_pending: false,
            lease,
        }
    }

    fn is_open(&self) -> bool {
        !self.cancelled && self.lease.upgrade().is_some_and(|open| open.get())
    }

    pub(super) fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.review_authorization.is_some() {
            self.review_focus.focus(window, cx);
            return;
        }
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    pub(super) fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending || !self.is_open() || self.device_auth_pending {
            return;
        }
        if let Some((scope, authorization)) = self.review_authorization.take() {
            let intent = self.intent.clone();
            self.root.update(cx, |root, cx| {
                if root.current_spend_authorization_scope() != scope
                    || !intent.approve_gateway_review(root)
                {
                    root.report_stale_private_fee_review(&intent, cx);
                    window.close_dialog(cx);
                    return;
                }
                window.close_dialog(cx);
                root.continue_authorized_spend(intent, authorization, window, cx);
            });
            return;
        }
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if password.trim().is_empty() {
            self.error = Some(Arc::from(
                "Enter the vault password to authorize this spend",
            ));
            cx.notify();
            return;
        }
        self.submit_password(password, self.lifetime, window, cx);
    }

    fn submit_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending
            || !self.is_open()
            || self.device_auth_pending
            || self.review_authorization.is_some()
        {
            return;
        }
        let Some(prompt) = self.device_auth.clone() else {
            return;
        };
        self.device_auth_pending = true;
        self.error = None;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_SPEND,
            window,
            cx,
            move |dialog, outcome, window, cx| {
                dialog.finish_device_auth(method, outcome, window, cx);
            },
        );
    }

    pub(super) fn finish_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_pending = false;
        if !self.is_open() {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => {
                // Watch approval is for this action only. It must never seed
                // the reusable password authorization cache.
                let lifetime = match method {
                    DeviceAuthMethod::AppleWatch => SpendAuthorizationLifetime::Once,
                    DeviceAuthMethod::TouchId => self.lifetime,
                };
                self.submit_password(password, lifetime, window, cx);
            }
            DeviceAuthPassword::Cancelled => self.focus_password(window, cx),
            DeviceAuthPassword::Failed(message) => {
                self.device_auth = self
                    .device_auth
                    .take()
                    .and_then(|prompt| prompt.without(method));
                self.error = Some(message);
                self.focus_password(window, cx);
            }
        }
        cx.notify();
    }

    fn submit_password(
        &mut self,
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.is_open() {
            return;
        }
        let root = self.root.read(cx);
        let Some(store) = root.vault_store.clone() else {
            self.error = Some("Wallet vault storage is unavailable".into());
            cx.notify();
            return;
        };
        let approved_scope = root.current_spend_authorization_scope();
        let approved_generation = root.active_wallet_generation;
        // Check the password before dismissing the review or starting an operation.
        // The operation still obtains its own scoped spend grant when it runs.
        let join = root.runtime.spawn_blocking(move || {
            store.create_spend_grant(&password).map(drop)?;
            WalletRoot::renew_device_auth(&store, &password);
            Ok::<_, VaultError>(password)
        });
        self.pending = true;
        self.error = None;
        let root = self.root.downgrade();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match join.await {
                Ok(result) => result.map_err(|error| match error {
                    VaultError::UnlockFailed => Arc::from("Incorrect vault password. Try again."),
                    error => error.to_string().into(),
                }),
                Err(_) => Err("Password check failed. Try again.".into()),
            };
            // Password verification can reseal device enrollments even after the review closes.
            let _ = root.update(cx, |root, cx| {
                root.refresh_device_auth_status();
                cx.notify();
            });
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                if !dialog.is_open() {
                    return;
                }
                let root = dialog.root.clone();
                if root.read(cx).active_wallet_generation != approved_generation
                    || root.read(cx).current_spend_authorization_scope() != approved_scope
                {
                    dialog.cancel(cx);
                    dialog.error = Some(
                        "The wallet session changed. Close this dialog and authorize the action again."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                match result {
                    Ok(password) => {
                        let intent = dialog.intent.clone();
                        if let Err(error) = root.update(cx, |root, cx| {
                            root.finish_spend_authorization(intent, password, lifetime, window, cx)
                        }) {
                            dialog.error = Some(error);
                            dialog.focus_password(window, cx);
                        }
                    }
                    Err(error) => {
                        root.update(cx, WalletRoot::clear_spend_authorization);
                        dialog.error = Some(error);
                        dialog.focus_password(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn cancel(&mut self, cx: &mut Context<'_, Self>) {
        self.cancelled = true;
        self.root.update(cx, |root, cx| {
            root.cancel_spend_authorization(&self.intent, cx);
        });
        cx.notify();
    }

    fn set_lifetime(&mut self, lifetime: SpendAuthorizationLifetime, cx: &mut Context<'_, Self>) {
        if !self.pending && !self.device_auth_pending && self.lifetime != lifetime {
            self.lifetime = lifetime;
            cx.notify();
        }
    }

    fn toggle_payload(&mut self, cx: &mut Context<'_, Self>) {
        self.payload_open = !self.payload_open;
        cx.notify();
    }

    fn toggle_row_group(&mut self, cx: &mut Context<'_, Self>) {
        self.row_group_open = !self.row_group_open;
        cx.notify();
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    /// The dialog's footer, which stays in view while the content scrolls.
    pub(super) fn render_footer(&self, dialog: &Entity<Self>) -> gpui::Div {
        let cancel_dialog = dialog.clone();
        let dialog = dialog.clone();
        div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-spend-auth-cancel", "Cancel")
                    .flex_none()
                    .on_click(move |_event, window, cx| {
                        cancel_dialog.update(cx, Self::cancel);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button(
                    "wallet-spend-auth-submit",
                    self.summary.confirm_label.to_string(),
                )
                .track_focus(&self.review_focus)
                .primary()
                .flex_none()
                .loading(self.pending)
                .disabled(self.pending || self.cancelled || self.device_auth_pending)
                .on_click(move |_event, window, cx| {
                    dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                }),
            )
    }
}

impl gpui::Render for SpendAuthorizationDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let payload_dialog = dialog.clone();
        let row_group_dialog = dialog.clone();
        let device_auth_dialog = dialog.clone();
        let payload = self.summary.payload.as_ref().map(|payload| {
            render_spend_authorization_payload(payload, self.payload_open, move |_, _, cx| {
                payload_dialog.update(cx, Self::toggle_payload);
            })
        });
        let row_group = self.render_row_group(row_group_dialog, cx);
        let details = self.render_details(dialog, cx);
        render_spend_authorization_dialog_body(&self.summary, row_group, details, cx)
            .children(
                self.summary
                    .warnings
                    .iter()
                    .enumerate()
                    .map(|(index, warning)| {
                        Alert::warning(
                            SharedString::from(format!("wallet-spend-auth-warning-{index}")),
                            warning.to_string(),
                        )
                        .small()
                    }),
            )
            .children(payload)
            .when(self.review_authorization.is_none(), |this| {
                self.render_password_entry(this, &device_auth_dialog)
            })
            .when(
                self.review_authorization.is_none()
                    && self.lifetime.requires_reusable_authorization_warning(),
                |this| {
                    this.child(
                        Alert::warning(
                            "wallet-spend-auth-session-warning",
                            SPEND_AUTHORIZATION_SESSION_WARNING,
                        )
                        .small(),
                    )
                },
            )
            .when_some(self.error.as_ref(), |this, error| {
                this.child(
                    app_muted_text(error.to_string())
                        .whitespace_normal()
                        .text_color(rgb(theme::DANGER))
                        .debug_selector(|| "wallet-spend-auth-error".into()),
                )
            })
    }
}

impl SpendAuthorizationDialogContent {
    /// The row group's disclosure, when the summary has one.
    fn render_row_group(&self, dialog: Entity<Self>, cx: &App) -> Option<AnyElement> {
        self.summary.row_group.as_ref().map(|group| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                group,
                self.row_group_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_row_group);
                },
                cx,
            )
            .into_any_element()
        })
    }

    /// The details disclosure, when the summary has one.
    fn render_details(&self, dialog: Entity<Self>, cx: &App) -> Option<AnyElement> {
        self.summary.details.as_ref().map(|details| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                details,
                self.details_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_details);
                },
                cx,
            )
            .into_any_element()
        })
    }

    /// The password input with its device-auth buttons, the lifetime row, and the Apple Watch
    /// note when Apple Watch is offered, added to `container`.
    fn render_password_entry(
        &self,
        container: gpui::Div,
        device_auth_dialog: &Entity<Self>,
    ) -> gpui::Div {
        let device_auth_dialog = device_auth_dialog.clone();
        container
            .child(masked_input_with_device_auth(
                &self.password_input,
                self.pending || self.cancelled || self.device_auth_pending,
                device_auth_buttons(
                    self.device_auth.as_ref(),
                    "wallet-spend-auth-touch-id",
                    self.device_auth_pending,
                    self.pending || self.cancelled,
                    move |method, window, cx| {
                        device_auth_dialog.update(cx, |dialog, cx| {
                            dialog.submit_with_device_auth(method, window, cx);
                        });
                    },
                ),
            ))
            .child(render_spend_authorization_lifetime_row(
                &self.lifetime_select,
                self.pending || self.cancelled || self.device_auth_pending,
            ))
            .when(
                self.device_auth
                    .as_ref()
                    .is_some_and(|prompt| prompt.includes(DeviceAuthMethod::AppleWatch)),
                |this| {
                    this.child(
                        app_muted_text("Apple Watch always authorizes only this spend.")
                            .text_xs()
                            .whitespace_normal(),
                    )
                },
            )
    }
}

/// The start of a spend authorization dialog's content: the steps, the detail, the summary,
/// the context and the notice.
fn render_spend_authorization_dialog_body(
    summary: &SpendAuthorizationSummary,
    row_group: Option<AnyElement>,
    details: Option<AnyElement>,
    cx: &App,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .children(summary.steps.as_ref().map(render_spend_authorization_steps))
        .when(!summary.detail.is_empty(), |this| {
            this.child(app_muted_text(summary.detail.to_string()).whitespace_normal())
        })
        .child(render_spend_authorization_summary(
            summary, row_group, details, cx,
        ))
        .when_some(summary.context.as_ref(), |this, context| {
            this.child(app_muted_text(context.to_string()).whitespace_normal())
        })
        .when_some(summary.notice.as_ref(), |this, (tone, text)| {
            this.child(app_notice(*tone, text.to_string(), cx))
        })
}

impl gpui::Render for HardwareSpendAuthorizationDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let payload_dialog = dialog.clone();
        let row_group_dialog = dialog.clone();
        let payload = self.summary.payload.as_ref().map(|payload| {
            render_spend_authorization_payload(payload, self.payload_open, move |_, _, cx| {
                payload_dialog.update(cx, Self::toggle_payload);
            })
        });
        let row_group = self.render_row_group(row_group_dialog, cx);
        let details = self.render_details(dialog, cx);
        let pending = self.pending;
        let device = self.device_label;
        let show_trezor_app_passphrase = self
            .root
            .read(cx)
            .current_session_needs_trezor_app_passphrase();
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase_input = self.root.read(cx).trezor_app_passphrase_input.clone();
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_prompt = self.render_trezor_pin_matrix_prompt(cx);
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_prompt: Option<AnyElement> = None;

        render_spend_authorization_dialog_body(&self.summary, row_group, details, cx)
            .children(self.summary.warnings.iter().enumerate().map(|(index, warning)| {
                Alert::warning(
                    SharedString::from(format!("wallet-hardware-spend-auth-warning-{index}")),
                    warning.to_string(),
                )
                .small()
            }))
            .children(payload)
            .child(Alert::warning(
                "wallet-hardware-spend-custody-warning",
                format!(
                    "Your {device} shows only a Railgun derivation request, not these details. Check them here before you approve."
                ),
            ).small())
            .when(show_trezor_app_passphrase, |this| {
                #[cfg(feature = "hardware")]
                {
                    this.child(render_trezor_app_passphrase(
                        &trezor_app_passphrase_input,
                        pending,
                    ))
                }
                #[cfg(not(feature = "hardware"))]
                {
                    this
                }
            })
            .children(trezor_pin_matrix_prompt)
            .when(pending, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(app_muted_shimmer_text(format!("Waiting for {device}…"))),
                )
            })
            .when_some(self.error.as_ref(), |this, error| {
                this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
            })
    }
}

impl HardwareSpendAuthorizationDialogContent {
    /// The row group's disclosure, when the summary has one.
    fn render_row_group(&self, dialog: Entity<Self>, cx: &App) -> Option<AnyElement> {
        self.summary.row_group.as_ref().map(|group| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                group,
                self.row_group_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_row_group);
                },
                cx,
            )
            .into_any_element()
        })
    }

    /// The details disclosure, when the summary has one.
    fn render_details(&self, dialog: Entity<Self>, cx: &App) -> Option<AnyElement> {
        self.summary.details.as_ref().map(|details| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                details,
                self.details_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_details);
                },
                cx,
            )
            .into_any_element()
        })
    }

    /// The Trezor PIN matrix, while the device asks for one.
    #[cfg(feature = "hardware")]
    fn render_trezor_pin_matrix_prompt(&self, cx: &App) -> Option<AnyElement> {
        let root = self.root.read(cx);
        root.hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_ref()
            .map(|prompt| {
                super::super::vault_ui::render_trezor_pin_matrix_prompt(&self.root, prompt)
                    .into_any_element()
            })
    }
}

/// The Trezor app passphrase prompt, shown when the Trezor session expired.
#[cfg(feature = "hardware")]
fn render_trezor_app_passphrase(input: &Entity<InputState>, pending: bool) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(app_strong_text("Trezor app passphrase"))
        .child(
            app_muted_text(
                "The Trezor session expired. Enter the passphrase for the wallet you intend to spend from.",
            )
            .whitespace_normal(),
        )
        .child(app_masked_input(input, pending))
}
