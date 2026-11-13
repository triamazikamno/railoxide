use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use super::DeliveryMode;
use super::auto_lock::AutoLockTimestamp;
use gpui::{AppContext, Context, Entity, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::{
    WindowExt,
    select::{SearchableVec, SelectItem, SelectState},
};
use ui::dialog::WalletDialogExt as _;
use wallet_ops::DesktopPrivateSpendAuthorization;
#[cfg(feature = "hardware")]
use wallet_ops::hardware::HardwareDeviceKind;
use wallet_ops::vault::{
    PublicAccountSource, SoftwareSeedSessionBinding, WalletSoftwareContextKind,
};
use zeroize::Zeroizing;

use super::{WalletRoot, dialog_max_height, secondary_dialog_content_width};

mod dialog;
mod hardware;
mod intent;
mod summary;
mod summary_view;

use dialog::SpendAuthorizationDialogContent;
pub(super) use hardware::HardwareSpendAuthorizationCompletion;
#[cfg(feature = "hardware")]
pub(super) use hardware::{HardwareSpendAuthorizationError, hardware_synthetic_entropy};
#[cfg(test)]
pub(super) use intent::private_fee_review_change;
pub(super) use intent::{PrivateFeeReview, SpendAuthorizationIntent, automatic_private_fee_bounds};
pub(super) use summary::{
    SpendAuthorizationCard, SpendAuthorizationHint, SpendAuthorizationLabel,
    SpendAuthorizationSteps, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    spend_authorization_can_use_cached_password,
};
use summary_view::spend_authorization_title;
pub(super) use summary_view::{
    render_spend_authorization_steps, spend_authorization_recipient_display,
};

const SPEND_AUTHORIZATION_DIALOG_WIDTH: gpui::Pixels = px(560.0);
const SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE: &str = "wallet-spend-auth-row-group-toggle";
const SPEND_AUTHORIZATION_DETAILS_TOGGLE: &str = "wallet-spend-auth-details-toggle";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SpendAuthorizationLifetime {
    Once,
    FiveMinutes,
    FifteenMinutes,
    UntilVaultLock,
}

type SpendAuthorizationLifetimeSelect = SelectState<SearchableVec<SpendAuthorizationLifetime>>;

impl SelectItem for SpendAuthorizationLifetime {
    type Value = Self;

    fn title(&self) -> SharedString {
        SharedString::from(self.label())
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

impl SpendAuthorizationLifetime {
    const ALL: [Self; 4] = [
        Self::Once,
        Self::FiveMinutes,
        Self::FifteenMinutes,
        Self::UntilVaultLock,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Once => "Just this spend",
            Self::FiveMinutes => "5 minutes",
            Self::FifteenMinutes => "15 minutes",
            Self::UntilVaultLock => "Until vault locks/app closes",
        }
    }

    const fn duration(self) -> Option<Duration> {
        match self {
            Self::Once | Self::UntilVaultLock => None,
            Self::FiveMinutes => Some(Duration::from_mins(5)),
            Self::FifteenMinutes => Some(Duration::from_mins(15)),
        }
    }

    pub(super) const fn requires_reusable_authorization_warning(self) -> bool {
        !matches!(self, Self::Once)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpendAuthorizationScope {
    base_profile_uuid: Arc<str>,
    wallet_uuid: Arc<str>,
    protected_seed_binding: Option<SoftwareSeedSessionBinding>,
}

impl SpendAuthorizationScope {
    fn new(
        base_profile_uuid: impl Into<Arc<str>>,
        wallet_uuid: impl Into<Arc<str>>,
        protected_seed_binding: Option<SoftwareSeedSessionBinding>,
    ) -> Self {
        Self {
            base_profile_uuid: base_profile_uuid.into(),
            wallet_uuid: wallet_uuid.into(),
            protected_seed_binding,
        }
    }
}

pub(super) struct SpendAuthorizationCache {
    password: Zeroizing<String>,
    scope: SpendAuthorizationScope,
    // Dual-clock so suspend time counts toward the lifetime.
    granted_at: AutoLockTimestamp,
    lifetime: Option<Duration>,
}

fn clear_protected_software_seed_session_state(
    protected_software_seed_session: &mut Option<
        Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
    >,
    spend_authorization_cache: &mut Option<SpendAuthorizationCache>,
) -> bool {
    let cleared = protected_software_seed_session.take().is_some();
    spend_authorization_cache.take().is_some() || cleared
}

impl SpendAuthorizationCache {
    fn new(
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        scope: SpendAuthorizationScope,
        now: AutoLockTimestamp,
    ) -> Option<Self> {
        match lifetime {
            SpendAuthorizationLifetime::Once => None,
            SpendAuthorizationLifetime::UntilVaultLock
            | SpendAuthorizationLifetime::FiveMinutes
            | SpendAuthorizationLifetime::FifteenMinutes => Some(Self {
                password,
                scope,
                granted_at: now,
                lifetime: lifetime.duration(),
            }),
        }
    }

    fn is_valid_at(&self, scope: &SpendAuthorizationScope, now: AutoLockTimestamp) -> bool {
        self.scope == *scope
            && self
                .lifetime
                .is_none_or(|lifetime| now.elapsed_since(self.granted_at) < lifetime)
    }
}

impl WalletRoot {
    pub(super) fn request_spend_authorization(
        &mut self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let summary = if self.selected_wallet_source().is_hardware_derived()
            && intent.hardware_executor_action(self).is_some()
        {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        let summary = if intent.gateway_execution().is_some()
            || matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        if self.selected_wallet_source().is_hardware_derived()
            && (intent.uses_private_wallet() || intent.hardware_executor_action(self).is_some())
        {
            intent.private_attention(
                "Approve on your hardware wallet",
                "Use the desktop app to approve this private spend on your device.",
            );
            self.clear_spend_authorization(cx);
            let key = match &intent {
                SpendAuthorizationIntent::PrepareExecutorUnshield(key, ..)
                | SpendAuthorizationIntent::ExecutorUnshield(key, ..) => Some(*key),
                _ => None,
            };
            let payer = if let SpendAuthorizationIntent::PublicShield(draft) = &intent
                && draft.native_operation.is_some()
            {
                // A stealth-route native shield's payer sends the stealth account's execution,
                // as a gas payer does. The draft admits only a software or imported payer here.
                Some(draft.public_account_uuid.to_string())
            } else if let Some(key) = key
                && let Some(draft) = self.unshield_spend_draft(key, cx)
                && draft.delivery_mode == DeliveryMode::SelfBroadcast
            {
                let Some(payer) = self.selected_self_broadcast_gas_payer_account(
                    draft.self_broadcast_public_account_uuid.as_deref(),
                ) else {
                    return;
                };
                if !matches!(
                    payer.source,
                    PublicAccountSource::Derived | PublicAccountSource::Imported
                ) {
                    self.set_unshield_form_error(key, "Select a software or imported gas payer, or a broadcaster, for this executor action.", cx);
                    return;
                }
                Some(payer.public_account_uuid.clone())
            } else {
                None
            };
            if let Some(payer) = payer {
                self.open_spend_authorization_dialog(
                    SpendAuthorizationIntent::ExecutorGasPassword {
                        intent: Box::new(intent),
                        summary: Box::new(summary.clone()),
                        payer,
                    },
                    summary.requiring_explicit_review(),
                    window,
                    cx,
                );
                return;
            }
            self.open_hardware_spend_authorization_dialog(
                HardwareSpendAuthorizationCompletion::Continue(intent),
                summary,
                window,
                cx,
            );
            return;
        }
        if spend_authorization_can_use_cached_password(&summary)
            && let Some(password) = self.valid_spend_authorization_password(cx)
        {
            match self.desktop_spend_authorization(password) {
                Ok(authorization) => {
                    if !intent.approve_gateway_review(self) {
                        return;
                    }
                    self.continue_authorized_spend(intent, authorization, window, cx);
                }
                Err(message) => self.set_vault_error(message, cx),
            }
            return;
        }

        intent.private_attention(
            "Authorize in the desktop app",
            "Enter your vault password in the desktop app to continue.",
        );
        self.open_spend_authorization_dialog(intent, summary, window, cx);
    }

    /// Counts a spend authorization dialog toward platform attention for as long as the
    /// returned lease lives. The dialog builder holds it, so any way of closing releases it.
    fn spend_authorization_attention_lease(window: &Window, cx: &mut Context<'_, Self>) -> Rc<()> {
        let lease = Rc::new(());
        let open = Rc::downgrade(&lease);
        cx.defer_in(window, move |root, window, _cx| {
            root.spend_authorization_attention.push(open);
            root.sync_platform_attention_for_window(window);
        });
        lease
    }

    fn open_spend_authorization_dialog(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_spend_authorization_dialog_with_review(intent, summary, None, window, cx);
    }

    pub(in crate::root) fn open_prepared_spend_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        intent.private_attention(
            "Review in the desktop app",
            "Review the updated transaction terms before signing.",
        );
        self.open_spend_authorization_dialog_with_review(
            intent,
            summary,
            Some((self.current_spend_authorization_scope(), authorization)),
            window,
            cx,
        );
    }

    fn open_spend_authorization_dialog_with_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        review_authorization: Option<(SpendAuthorizationScope, DesktopPrivateSpendAuthorization)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SpendAuthorizationDialogContent> {
        let root = cx.entity();
        let initial_lifetime = self.spend_authorization_lifetime;
        let dialog_title = summary.title.to_string();
        let title_network = summary.title_network;
        let device_auth = self.device_auth_prompt_cached();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let attention = Self::spend_authorization_attention_lease(window, cx);
        let content = cx.new(|cx| {
            let mut content = SpendAuthorizationDialogContent::new(
                root,
                intent,
                summary,
                initial_lifetime,
                identity,
                window,
                cx,
            );
            content.review_authorization = review_authorization;
            content.device_auth = device_auth;
            content
        });
        let focus_content = content.clone();
        let dialog_content = content.clone();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let close_content = dialog_content.clone();
            let identity = Rc::downgrade(&lease);
            let _ = &attention;
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height(window))
                .title(spend_authorization_title(&dialog_title, title_network))
                .on_close(move |_event, _window, cx| {
                    if let Some(open) = identity.upgrade() {
                        open.set(false);
                    }
                    close_content.update(cx, SpendAuthorizationDialogContent::cancel);
                })
                .child(div().w(content_width).child(dialog_content.clone()))
                // The footer stays in view while the content scrolls.
                .footer(
                    dialog_content
                        .read(cx)
                        .render_footer(&dialog_content)
                        .w(content_width),
                )
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
        content
    }

    fn valid_spend_authorization_password(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        let now = AutoLockTimestamp::now();
        let scope = self.current_spend_authorization_scope();
        if self
            .spend_authorization_cache
            .as_ref()
            .is_some_and(|authorization| authorization.is_valid_at(&scope, now))
        {
            return self
                .spend_authorization_cache
                .as_ref()
                .map(|authorization| authorization.password.clone());
        }
        if self.spend_authorization_cache.take().is_some() {
            cx.notify();
        }
        None
    }

    fn cancel_spend_authorization(
        &mut self,
        intent: &SpendAuthorizationIntent,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(execution) = intent.gateway_execution() {
            execution.cancel_review();
            self.release_gateway_private_form(execution, cx);
        }
        self.cancel_governance_authorization(intent, cx);
        if let SpendAuthorizationIntent::StealthAccounts(view, command) = intent {
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| view.cancel_authorization(&command, cx));
            });
        }
        if let SpendAuthorizationIntent::PrivateSwap(view, command) = intent {
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| view.cancel_authorization(&command, cx));
            });
        }
        if let SpendAuthorizationIntent::PublicSwap(view, command)
        | SpendAuthorizationIntent::PublicSwapSource { view, command, .. } = intent
        {
            if let SpendAuthorizationIntent::PublicSwapSource {
                private_authorization,
                ..
            } = intent
            {
                private_authorization.borrow_mut().take();
            }
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| {
                    view.cancel_public_authorization(&command, cx);
                });
            });
        }
    }

    /// Explains on the form why a private review with a changed fee was not submitted.
    fn report_stale_private_fee_review(
        &mut self,
        intent: &SpendAuthorizationIntent,
        cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        let message = intent.private_fee_change(self)?;
        let intent = match intent {
            SpendAuthorizationIntent::ExecutorGasPassword { intent, .. } => intent.as_ref(),
            intent => intent,
        };
        // Set the error directly: the form setters would also discard the current or refreshing estimate.
        let error = match intent {
            SpendAuthorizationIntent::PrivateSend(key, ..) => {
                self.send_forms.get_mut(key).map(|form| &mut form.error)
            }
            SpendAuthorizationIntent::PrivateUnshield(key, ..) => {
                self.unshield_forms.get_mut(key).map(|form| &mut form.error)
            }
            _ => None,
        };
        if let Some(error) = error {
            *error = Some(message.as_str().into());
            cx.notify();
        }
        Some(message)
    }

    pub(super) fn finish_spend_authorization(
        &mut self,
        intent: SpendAuthorizationIntent,
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        let authorization = self.desktop_spend_authorization(password.clone())?;
        if !intent.approve_gateway_review(self) {
            if matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
                self.cancel_spend_authorization(&intent, cx);
            }
            self.report_stale_private_fee_review(&intent, cx);
            window.close_dialog(cx);
            return Ok(());
        }
        self.spend_authorization_lifetime = lifetime;
        self.spend_authorization_cache = SpendAuthorizationCache::new(
            password,
            lifetime,
            self.current_spend_authorization_scope(),
            AutoLockTimestamp::now(),
        );
        window.close_dialog(cx);
        self.continue_authorized_spend(intent, authorization, window, cx);
        Ok(())
    }

    pub(super) fn clear_spend_authorization(&mut self, cx: &mut Context<'_, Self>) {
        if self.spend_authorization_cache.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn clear_protected_software_seed_session(&mut self, cx: &mut Context<'_, Self>) {
        if self.protected_software_seed_session.is_some() {
            // A passphrase context's address preview was authorized with this session.
            self.public_form.derive_preview.reset();
        }
        if clear_protected_software_seed_session_state(
            &mut self.protected_software_seed_session,
            &mut self.spend_authorization_cache,
        ) {
            cx.notify();
        }
    }

    fn current_spend_authorization_scope(&self) -> SpendAuthorizationScope {
        let wallet_uuid = self.selected_wallet_id.as_deref().unwrap_or("");
        let base_profile_uuid = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
            .and_then(|metadata| metadata.software_context.as_ref())
            .map_or(wallet_uuid, |context| context.base_profile_uuid.as_str());
        SpendAuthorizationScope::new(
            base_profile_uuid,
            wallet_uuid,
            self.protected_software_seed_session
                .as_ref()
                .map(|session| session.binding().clone()),
        )
    }

    fn desktop_spend_authorization(
        &self,
        password: Zeroizing<String>,
    ) -> Result<DesktopPrivateSpendAuthorization, Arc<str>> {
        let wallet_uuid = self
            .selected_wallet_id
            .as_deref()
            .ok_or_else(|| Arc::from("Select a wallet before authorizing a spend"))?;
        let metadata = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
            .ok_or_else(|| Arc::from("Selected wallet metadata is unavailable"))?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Ok(DesktopPrivateSpendAuthorization::VaultPassword(password));
        };
        if context.kind != WalletSoftwareContextKind::Passphrase {
            return Ok(DesktopPrivateSpendAuthorization::VaultPassword(password));
        }
        let session = self
            .protected_software_seed_session
            .as_ref()
            .ok_or_else(|| {
                Arc::from("Open the selected passphrase wallet again before spending")
            })?;
        if session.binding().base_profile_uuid() != context.base_profile_uuid
            || session.binding().context_wallet_uuid() != wallet_uuid
        {
            return Err(Arc::from(
                "The selected passphrase wallet session is stale; open it again before spending",
            ));
        }
        Ok(DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
            password,
            session: Arc::clone(session),
        })
    }

    pub(super) fn continue_authorized_spend(
        &mut self,
        intent: SpendAuthorizationIntent,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.continue_authorized_spend_with_destination(intent, authorization, None, window, cx);
    }

    /// [`Self::continue_authorized_spend`] with `destination`, the authorization a hardware
    /// wallet's device session also gave for a private Bridge swap's destination network. Only
    /// a private swap takes one.
    fn continue_authorized_spend_with_destination(
        &mut self,
        intent: SpendAuthorizationIntent,
        authorization: DesktopPrivateSpendAuthorization,
        destination: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !intent.private_review_current(self) {
            if matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
                self.cancel_spend_authorization(&intent, cx);
            }
            return;
        }
        match intent {
            SpendAuthorizationIntent::ExecutorGasPassword {
                intent,
                summary,
                payer,
            } => {
                let seed_session = authorization.protected_seed_session();
                let (DesktopPrivateSpendAuthorization::VaultPassword(password)
                | DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed { password, .. }) =
                    authorization
                else {
                    self.set_vault_error(
                        "Authorize the selected software gas payer with its vault password",
                        cx,
                    );
                    return;
                };
                self.clear_spend_authorization(cx);
                self.open_hardware_spend_authorization_dialog(
                    HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer {
                        intent: *intent,
                        payer,
                        password,
                        seed_session,
                    },
                    *summary,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::StealthAccounts(view, command) => {
                // The panel reads WalletRoot to validate its session. Release this update first.
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized(command, authorization, window, cx);
                    });
                });
            }
            SpendAuthorizationIntent::PrivateSwap(view, command) => {
                // The swap view reads WalletRoot to validate its session. Release this update first.
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized(&command, authorization, destination, window, cx);
                    });
                });
            }
            SpendAuthorizationIntent::PublicSwap(view, command) => {
                let public_authorization = match command.source().source {
                    PublicAccountSource::HardwareDerived => None,
                    PublicAccountSource::Derived | PublicAccountSource::Imported => {
                        match &authorization {
                            DesktopPrivateSpendAuthorization::VaultPassword(password) => Some(
                                DesktopPrivateSpendAuthorization::VaultPassword(password.clone()),
                            ),
                            DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
                                password,
                                session,
                            } => Some(DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
                                password: password.clone(),
                                session: Arc::clone(session),
                            }),
                            _ => None,
                        }
                    }
                    PublicAccountSource::ExecutorDerived(_) => {
                        self.cancel_spend_authorization(
                            &SpendAuthorizationIntent::PublicSwap(view, command),
                            cx,
                        );
                        return;
                    }
                };
                if let Some(public_authorization) = public_authorization {
                    window.defer(cx, move |window, cx| {
                        view.update(cx, |view, cx| {
                            view.continue_authorized_public_swap(
                                &command,
                                authorization,
                                Some(public_authorization),
                                None,
                                None,
                                window,
                                cx,
                            );
                        });
                    });
                } else {
                    let hardware_public =
                        command.source().source == PublicAccountSource::HardwareDerived;
                    // The review was approved for the wallet's device, which also holds this
                    // account. A second dialog would repeat it and prompt nothing: the device
                    // asks when it signs. Only an app passphrase still has to be entered.
                    if hardware_public && !self.current_session_needs_trezor_app_passphrase() {
                        #[cfg(feature = "hardware")]
                        let trezor_pin_matrix_provider = self
                            .view_session
                            .as_ref()
                            .and_then(|view| view.hardware_profile_session())
                            .filter(|session| session.device_kind == HardwareDeviceKind::Trezor)
                            .cloned()
                            .map(|_| self.trezor_pin_matrix_provider_for_operation(window, cx));
                        #[cfg(not(feature = "hardware"))]
                        let trezor_pin_matrix_provider = None;
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.continue_authorized_public_swap(
                                    &command,
                                    authorization,
                                    Some(DesktopPrivateSpendAuthorization::HardwarePublic),
                                    None,
                                    trezor_pin_matrix_provider,
                                    window,
                                    cx,
                                );
                            });
                        });
                        return;
                    }
                    let summary = command.public_authorization_summary();
                    let intent = SpendAuthorizationIntent::PublicSwapSource {
                        view,
                        command,
                        private_authorization: Rc::new(RefCell::new(Some(authorization))),
                    };
                    if hardware_public {
                        Self::open_hardware_public_action_authorization_dialog(
                            intent, summary, window, cx,
                        );
                    } else {
                        self.request_spend_authorization(intent, summary, window, cx);
                    }
                }
            }
            SpendAuthorizationIntent::PublicSwapSource {
                view,
                command,
                private_authorization,
            } => {
                let Some(private_authorization) = private_authorization.borrow_mut().take() else {
                    return;
                };
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized_public_swap(
                            &command,
                            private_authorization,
                            Some(authorization),
                            None,
                            None,
                            window,
                            cx,
                        );
                    });
                });
            }
            SpendAuthorizationIntent::PrepareExecutorUnshield(key, approval, execution) => {
                self.prepare_executor_unshield_review(key, approval, authorization, window, cx);
                if !self
                    .unshield_forms
                    .get(&key)
                    .is_some_and(|form| form.generating)
                    && let Some(execution) = execution
                {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::ExecutorUnshield(key, review, execution) => {
                let current = self.unshield_spend_draft(key, cx);
                if current.as_ref().is_some_and(|draft| review.matches(draft)) {
                    self.generate_unshield_calldata_authorized(
                        key,
                        authorization,
                        None,
                        None,
                        window,
                        cx,
                    );
                } else {
                    self.set_unshield_form_error(
                        key,
                        "The prepared action changed. Refresh and review it again.",
                        cx,
                    );
                }
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateSend(
                key,
                authorization_limit,
                execution,
                _,
                fee_review,
            ) => {
                self.generate_send_calldata_authorized(
                    key,
                    authorization,
                    authorization_limit,
                    fee_review.as_deref(),
                    window,
                    cx,
                );
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateSendSelfBroadcastGasPassword(
                key,
                authorization_limit,
                execution,
            ) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Self-broadcast software gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_private_send_hardware_authorization_with_gas_password(
                    key,
                    password,
                    authorization_limit,
                    execution,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::PrivateUnshield(
                key,
                authorization_limit,
                execution,
                _,
                fee_review,
            ) => {
                self.generate_unshield_calldata_authorized(
                    key,
                    authorization,
                    authorization_limit,
                    fee_review.as_deref(),
                    window,
                    cx,
                );
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateUnshieldSelfBroadcastGasPassword(
                key,
                authorization_limit,
                execution,
            ) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Self-broadcast software gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_private_unshield_hardware_authorization_with_gas_password(
                    key,
                    password,
                    authorization_limit,
                    execution,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::BlockedShieldRefund(utxo_id) => {
                self.submit_blocked_shield_refund_authorized(
                    utxo_id,
                    authorization,
                    None,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::BlockedShieldRefundGasPassword(utxo_id) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Blocked Shield refund gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_blocked_shield_refund_hardware_authorization(
                    utxo_id, password, window, cx,
                );
            }
            SpendAuthorizationIntent::PublicSend(draft) => {
                self.submit_public_send_authorized(*draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::PublicShield(draft) => {
                self.submit_public_shield_authorized(*draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::TronSend(intent) => {
                self.submit_tron_send_authorized(*intent, authorization, window, cx);
            }
            SpendAuthorizationIntent::Governance(draft) => {
                self.revalidate_governance_authorized(&draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::WalletConnectRequest {
                request_key,
                review_token,
                reviewed_fee,
            } => {
                self.submit_walletconnect_request_authorized(
                    &request_key,
                    review_token,
                    reviewed_fee,
                    authorization,
                    window,
                    cx,
                );
            }
        }
    }
}

pub(super) fn is_spend_authorization_failure_error(error: &str) -> bool {
    error.contains("authorize ") && error.ends_with("unlock failed")
}

#[cfg(test)]
pub(super) fn remembered_spend_authorization_valid_for_test(
    lifetime: SpendAuthorizationLifetime,
    elapsed: Duration,
) -> bool {
    let now = AutoLockTimestamp::now();
    let scope = SpendAuthorizationScope::new("base", "wallet", None);
    let Some(cache) = SpendAuthorizationCache::new(
        Zeroizing::new("password".to_string()),
        lifetime,
        scope.clone(),
        now,
    ) else {
        return false;
    };
    cache.is_valid_at(&scope, now + elapsed)
}

#[cfg(test)]
mod tests;
