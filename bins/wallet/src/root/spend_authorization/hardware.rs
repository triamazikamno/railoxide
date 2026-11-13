use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _,
};
#[cfg(feature = "hardware")]
use gpui::{px, rgb};
use gpui_component::{Sizable, alert::Alert};
#[cfg(feature = "hardware")]
use ui::controls::app_masked_input;
use ui::controls::{app_muted_text, app_strong_text};
use ui::dialog::WalletDialogExt as _;
#[cfg(feature = "hardware")]
use ui::theme;
use wallet_ops::hardware::HardwareDerivationDescriptor;
#[cfg(feature = "hardware")]
use wallet_ops::hardware::{
    HardwareDerivationError, HardwareDeviceKind,
    ledger::LedgerHardwareDerivationClient,
    synthetic_entropy_from_hardware_output,
    trezor::{TrezorHardwareDerivationClient, TrezorPinMatrixProvider},
};
#[cfg(feature = "hardware")]
use wallet_ops::vault::{
    DesktopVaultStore, DesktopViewSession, HardwareProfileSession, VaultError,
};
use wallet_ops::{
    BlockedShieldRescueUtxoId, DesktopPrivateSpendAuthorization, SponsoredAuthorizationLimit,
};
use zeroize::Zeroizing;

use crate::root::ui_helpers::dialog_footer;

use super::super::private_action::UnshieldAssetKey;
use super::super::vault::hardware_device_label;
use super::super::{WalletRoot, dialog_max_height, secondary_dialog_content_width};
use super::dialog::{HardwareGasPaymentReview, HardwareSpendAuthorizationDialogContent};
use super::intent::SpendAuthorizationIntent;
use super::summary::SpendAuthorizationSummary;
use super::summary_view::{
    SpendAuthorizationDetailsDisclosure, SpendAuthorizationPayloadDisclosure,
    render_spend_authorization_steps, render_spend_authorization_summary,
    spend_authorization_title,
};
use super::{
    SPEND_AUTHORIZATION_DETAILS_TOGGLE, SPEND_AUTHORIZATION_DIALOG_WIDTH,
    SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
};

#[derive(Clone)]
#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
pub(in crate::root) enum HardwareSpendAuthorizationCompletion {
    Continue(SpendAuthorizationIntent),
    ExecutorWithGasPayer {
        intent: SpendAuthorizationIntent,
        payer: String,
        password: Zeroizing<String>,
        seed_session: Option<Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>>,
    },
    PrivateSendSelfBroadcast {
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
    },
    PrivateUnshieldSelfBroadcast {
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
    },
    BlockedShieldRefund {
        utxo_id: BlockedShieldRescueUtxoId,
        vault_password: Zeroizing<String>,
    },
}

impl HardwareSpendAuthorizationCompletion {
    pub(super) fn private_intent(&self) -> Option<SpendAuthorizationIntent> {
        match self {
            Self::Continue(intent) | Self::ExecutorWithGasPayer { intent, .. } => {
                Some(intent.clone())
            }
            Self::PrivateSendSelfBroadcast {
                key,
                authorization_limit,
                execution,
                ..
            } => Some(SpendAuthorizationIntent::PrivateSend(
                *key,
                *authorization_limit,
                execution.clone(),
                None,
                None,
            )),
            Self::PrivateUnshieldSelfBroadcast {
                key,
                authorization_limit,
                execution,
                ..
            } => Some(SpendAuthorizationIntent::PrivateUnshield(
                *key,
                *authorization_limit,
                execution.clone(),
                None,
                None,
            )),
            Self::BlockedShieldRefund { .. } => None,
        }
    }
}

#[cfg(feature = "hardware")]
pub(in crate::root) enum HardwareSpendAuthorizationError {
    Hardware(HardwareDerivationError),
    Vault(VaultError),
    Executor(String),
}

/// The authorization, a private Bridge swap's second one for its destination network from the
/// same device session, and the refreshed hardware session.
#[cfg(feature = "hardware")]
type HardwareSpendAuthorizationTaskOutput = Result<
    (
        DesktopPrivateSpendAuthorization,
        Option<DesktopPrivateSpendAuthorization>,
        HardwareProfileSession,
    ),
    HardwareSpendAuthorizationError,
>;

#[cfg(feature = "hardware")]
impl From<HardwareDerivationError> for HardwareSpendAuthorizationError {
    fn from(error: HardwareDerivationError) -> Self {
        Self::Hardware(error)
    }
}

#[cfg(feature = "hardware")]
impl From<VaultError> for HardwareSpendAuthorizationError {
    fn from(error: VaultError) -> Self {
        Self::Vault(error)
    }
}

impl WalletRoot {
    pub(in crate::root) fn open_hardware_public_action_authorization_dialog(
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        let payload_disclosure = summary
            .payload
            .clone()
            .map(|payload| cx.new(|_cx| SpendAuthorizationPayloadDisclosure::new(payload)));
        let row_group_disclosure = summary.row_group.clone().map(|group| {
            cx.new(|_cx| {
                SpendAuthorizationDetailsDisclosure::new(
                    SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                    group,
                )
            })
        });
        let details_disclosure = summary.details.clone().map(|details| {
            cx.new(|_cx| {
                SpendAuthorizationDetailsDisclosure::new(
                    SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                    details,
                )
            })
        });
        let handed_off = Rc::new(Cell::new(false));
        let attention = Self::spend_authorization_attention_lease(window, cx);
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let _ = &attention;
            let close_root = root.clone();
            let submit_root = root.clone();
            let close_intent = intent.clone();
            let show_trezor_app_passphrase = root
                .read(cx)
                .current_session_needs_trezor_app_passphrase();
            #[cfg(feature = "hardware")]
            let trezor_app_passphrase_input = root.read(cx).trezor_app_passphrase_input.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height(window))
                .title(app_strong_text("Authorize hardware public action"))
                .footer(dialog_footer("Approve on device", true))
                .on_close({
                    let handed_off = handed_off.clone();
                    move |_event, window, cx| {
                        close_root.update(cx, |root, cx| {
                            root.clear_trezor_app_passphrase_input(window, cx);
                            if !handed_off.get() {
                                root.cancel_spend_authorization(&close_intent, cx);
                            }
                        });
                    }
                })
                .on_ok({
                    let intent = intent.clone();
                    let handed_off = handed_off.clone();
                    move |_event, window, cx| {
                        let intent = intent.clone();
                        if !intent.approve_gateway_review(submit_root.read(cx)) { return true; }
                        handed_off.set(true);
                        submit_root.update(cx, |root, cx| {
                            if let SpendAuthorizationIntent::PublicSwapSource {
                                view,
                                command,
                                private_authorization,
                            } = intent
                            {
                                let Some(private_authorization) = private_authorization.borrow_mut().take() else {
                                    return;
                                };
                                // Closing this dialog clears the passphrase input before the deferred handoff.
                                #[cfg(feature = "hardware")]
                                let (trezor_app_passphrase, trezor_pin_matrix_provider) = {
                                    let session = root.view_session.as_ref()
                                        .and_then(|view| view.hardware_profile_session()).cloned();
                                    let passphrase = session.as_ref().and_then(|session| {
                                        root.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
                                    });
                                    let pin = session.as_ref()
                                        .filter(|session| session.device_kind == HardwareDeviceKind::Trezor)
                                        .map(|_| root.trezor_pin_matrix_provider_for_operation(window, cx));
                                    (passphrase, pin)
                                };
                                #[cfg(not(feature = "hardware"))]
                                let (trezor_app_passphrase, trezor_pin_matrix_provider) = (None, None);
                                window.defer(cx, move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.continue_authorized_public_swap(
                                            &command,
                                            private_authorization,
                                            Some(DesktopPrivateSpendAuthorization::HardwarePublic),
                                            trezor_app_passphrase,
                                            trezor_pin_matrix_provider,
                                            window,
                                            cx,
                                        );
                                    });
                                });
                            } else {
                                root.continue_authorized_spend(
                                    intent,
                                    DesktopPrivateSpendAuthorization::HardwarePublic,
                                    window,
                                    cx,
                                );
                            }
                        });
                        true
                    }
                })
                .child(div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(spend_authorization_title(&summary.title, summary.title_network))
                    .children(summary.steps.as_ref().map(render_spend_authorization_steps))
                    .child(app_muted_text(summary.detail.to_string()).whitespace_normal())
                    .child(render_spend_authorization_summary(
                        &summary,
                        row_group_disclosure.clone().map(IntoElement::into_any_element),
                        details_disclosure.clone().map(IntoElement::into_any_element),
                        cx,
                    ))
                    .children(summary.warnings.iter().enumerate().map(|(index, warning)| {
                        Alert::warning(
                            SharedString::from(format!(
                                "wallet-hardware-public-action-warning-{index}"
                            )),
                            warning.to_string(),
                        )
                        .small()
                    }))
                    .children(payload_disclosure.clone())
                    .when(show_trezor_app_passphrase, |this| {
                        #[cfg(feature = "hardware")]
                        {
                            this.child(
                                div()
                                    .w_full()
                                    .p(px(12.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(theme::BORDER))
                                    .bg(rgb(theme::SURFACE))
                                    .child(app_strong_text("Trezor app passphrase"))
                                    .child(
                                        app_muted_text(
                                            "If the Trezor session expired, enter the app passphrase for this public account request.",
                                        )
                                        .whitespace_normal(),
                                    )
                                    .child(app_masked_input(&trezor_app_passphrase_input, false)),
                            )
                        }
                        #[cfg(not(feature = "hardware"))]
                        {
                            this
                        }
                    })
                    .child(
                        app_muted_text("The app will verify the stored public account address against the connected device before signing.")
                            .whitespace_normal(),
                    ))
        });
    }

    pub(super) fn hardware_gas_payment_review(
        &mut self,
        completion: &HardwareSpendAuthorizationCompletion,
        cx: &mut Context<'_, Self>,
    ) -> Option<HardwareGasPaymentReview> {
        let intent = completion.private_intent()?;
        if !matches!(
            intent.hardware_executor_action(self),
            Some(wallet_ops::HardwareExecutorAction::GasPayment { .. })
        ) {
            return None;
        }
        match intent {
            SpendAuthorizationIntent::PrivateSend(key, ..) => {
                let draft = self.send_spend_draft(key, cx)?;
                Some(HardwareGasPaymentReview {
                    form: self.send_forms.get(&key)?.recipient_input.entity_id(),
                    recipient: draft.recipient,
                    amount: draft.amount,
                    payer: draft.self_broadcast_public_account_uuid,
                    funding: draft.self_broadcast_funding,
                    gas_fee: draft.self_broadcast_gas_fee,
                    incentive: draft.sponsored_incentive,
                    fee_mode: draft.fee_mode,
                    unwrap: false,
                    top_up: None,
                })
            }
            SpendAuthorizationIntent::PrivateUnshield(key, ..) => {
                let draft = self.unshield_spend_draft(key, cx)?;
                Some(HardwareGasPaymentReview {
                    form: self.unshield_forms.get(&key)?.recipient_input.entity_id(),
                    recipient: draft.recipient.to_string(),
                    amount: draft.amount,
                    payer: draft.self_broadcast_public_account_uuid,
                    funding: draft.self_broadcast_funding,
                    gas_fee: draft.self_broadcast_gas_fee,
                    incentive: draft.sponsored_incentive,
                    fee_mode: draft.fee_mode,
                    unwrap: draft.unwrap,
                    top_up: draft.native_top_up,
                })
            }
            _ => None,
        }
    }

    pub(in crate::root) fn open_hardware_spend_authorization_dialog(
        &mut self,
        completion: HardwareSpendAuthorizationCompletion,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(descriptor) = self.selected_hardware_descriptor() else {
            self.set_vault_error(
                "Selected wallet is missing its hardware derivation descriptor",
                cx,
            );
            return;
        };
        let root = cx.entity();
        let device_label = hardware_device_label(descriptor.device_kind);
        let gas_review = self.hardware_gas_payment_review(&completion, cx);
        let dialog_title = summary.title.to_string();
        let title_network = summary.title_network;
        let content = cx.new(|_cx| {
            HardwareSpendAuthorizationDialogContent::new(
                root.clone(),
                completion,
                gas_review,
                summary,
                device_label,
            )
        });
        let attention = Self::spend_authorization_attention_lease(window, cx);
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let _ = &attention;
            let close_content = content.clone();
            let close_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height(window))
                .title(spend_authorization_title(&dialog_title, title_network))
                .on_ok({
                    let content = content.clone();
                    move |_event, window, cx| {
                        content.update(cx, |content, cx| content.start(window, cx));
                        false
                    }
                })
                .on_close(move |_event, window, cx| {
                    close_content.update(cx, HardwareSpendAuthorizationDialogContent::cancel);
                    close_root.update(cx, |root, cx| {
                        root.clear_trezor_app_passphrase_input(window, cx);
                        root.clear_trezor_pin_matrix_prompt(cx);
                    });
                })
                .child(div().w(content_width).child(content.clone()))
                // The footer stays in view while the content scrolls.
                .footer(content.read(cx).render_footer(&content).w(content_width))
        });
    }

    pub(in crate::root) fn selected_hardware_descriptor(
        &self,
    ) -> Option<HardwareDerivationDescriptor> {
        let selected_wallet_id = self.selected_wallet_id.as_ref()?;
        self.wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == selected_wallet_id.as_ref())
            .and_then(|metadata| metadata.hardware_derivation_descriptor().cloned())
    }

    #[cfg(feature = "hardware")]
    pub(super) fn start_hardware_spend_authorization_task(
        &mut self,
        completion: &HardwareSpendAuthorizationCompletion,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<tokio::task::JoinHandle<HardwareSpendAuthorizationTaskOutput>, Arc<str>> {
        if let Some(intent @ SpendAuthorizationIntent::PublicSwap(..)) = completion.private_intent()
            && !intent.private_review_current(self)
        {
            return Err(Arc::from(
                "The swap source or destination changed. Close this dialog and review the swap again.",
            ));
        }
        let Some(descriptor) = self.selected_hardware_descriptor() else {
            return Err(Arc::from(
                "Selected wallet is missing its hardware derivation descriptor",
            ));
        };
        let Some(store) = self.vault_store.clone() else {
            return Err(Arc::from("Wallet vault storage is unavailable"));
        };
        let Some(view_session) = self.view_session.clone() else {
            return Err(Arc::from(
                "Unlock the wallet vault before authorizing a spend",
            ));
        };
        let Some(hardware_session) = view_session.hardware_profile_session().cloned() else {
            return Err(Arc::from(
                "Unlock the matching hardware profile before authorizing a spend",
            ));
        };
        let executor_action = match completion.private_intent() {
            Some(SpendAuthorizationIntent::WalletConnectRequest {
                request_key,
                review_token,
                ..
            }) => {
                let action = self
                    .walletconnect_hardware_executor_action(&request_key, review_token)
                    .ok_or_else(|| {
                        Arc::<str>::from(
                            "The request changed or its chain is unavailable. Review it again.",
                        )
                    })?;
                Some(action)
            }
            Some(intent) => {
                // A swap's setup sent again on its destination network is approved by that
                // network's owner.
                let chain_id = match &intent {
                    SpendAuthorizationIntent::PrivateSwap(_, command) => {
                        command.hardware_executor_chain()
                    }
                    SpendAuthorizationIntent::PublicSwap(_, command) => {
                        command.hardware_executor_chain()
                    }
                    _ => self.last_evm_chain,
                };
                intent
                    .hardware_executor_action(self)
                    .map(|action| (chain_id, action))
            }
            None => None,
        };
        let executor_request = executor_action
            .map(|(chain_id, action)| {
                self.executor_owner_for_public_chain(chain_id)
                    .ok_or_else(|| {
                        Arc::<str>::from(
                            "Open the wallet and chain before authorizing this account",
                        )
                    })?
                    .hardware_authorization_request(Arc::clone(&view_session), action)
                    .map_err(|error| Arc::<str>::from(error.to_string()))
            })
            .transpose()?;
        // A private Bridge swap also signs for its stealth account on the destination network,
        // approved in the same device session.
        let destination_request = match completion.private_intent() {
            Some(SpendAuthorizationIntent::PrivateSwap(_, command)) => command
                .hardware_destination_action()
                .map(|(chain_id, action)| {
                    self.executor_owner_for_public_chain(chain_id)
                        .ok_or_else(|| {
                            Arc::<str>::from(
                                "Wait for the swap's destination network to load before authorizing this swap",
                            )
                        })?
                        .hardware_authorization_request(Arc::clone(&view_session), action)
                        .map_err(|error| Arc::<str>::from(error.to_string()))
                })
                .transpose()?,
            _ => None,
        };
        let gas_payer = if let HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer {
            payer,
            password,
            seed_session,
            ..
        } = completion
        {
            Some((payer.clone(), password.clone(), seed_session.clone()))
        } else {
            None
        };
        let trezor_app_passphrase =
            self.read_trezor_app_passphrase_for_hardware_session(&hardware_session, window, cx);
        let trezor_pin_matrix_provider =
            if hardware_session.device_kind == HardwareDeviceKind::Trezor {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        Ok(self.runtime.spawn(async move {
            let executor_request = if let Some((payer, password, seed_session)) = gas_payer {
                let request = executor_request.ok_or_else(|| {
                    HardwareSpendAuthorizationError::Executor(
                        "Executor approval is unavailable".into(),
                    )
                })?;
                Some(
                    tokio::task::spawn_blocking(move || {
                        request.with_gas_payer(payer, password, seed_session)
                    })
                    .await
                    .map_err(|_| {
                        HardwareSpendAuthorizationError::Executor(
                            "Gas-payer authorization task failed. Try again.".into(),
                        )
                    })?
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?,
                )
            } else {
                executor_request
            };
            derive_hardware_spend_authorization(
                store,
                view_session,
                hardware_session,
                descriptor,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
                executor_request.map(|request| (request, destination_request)),
            )
            .await
        }))
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn refresh_active_hardware_profile_session(
        &mut self,
        hardware_session: HardwareProfileSession,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view_session) = self.view_session.as_ref() else {
            return;
        };
        if view_session.hardware_profile_session().is_none() {
            return;
        }
        let refreshed =
            Arc::new(view_session.clone_with_hardware_profile_session(hardware_session));
        self.gateway.drafts.borrow_mut().refresh_hardware_session(
            view_session,
            &refreshed,
            self.active_wallet_generation,
        );
        self.public_form
            .derive_preview
            .end_hardware_preview_unless(refreshed.hardware_profile_session());
        self.view_session = Some(refreshed);
        cx.notify();
    }

    pub(super) fn request_private_send_hardware_authorization_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut draft) = self.send_spend_draft(key, cx) else {
            if let Some(execution) = execution {
                self.reject_gateway_private_authorization(&execution, cx);
            }
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        self.open_hardware_spend_authorization_dialog(
            HardwareSpendAuthorizationCompletion::PrivateSendSelfBroadcast {
                key,
                vault_password,
                authorization_limit,
                execution,
            },
            super::super::private_action::private_send_authorization_summary(&draft),
            window,
            cx,
        );
    }

    pub(super) fn request_private_unshield_hardware_authorization_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut draft) = self.unshield_spend_draft(key, cx) else {
            if let Some(execution) = execution {
                self.reject_gateway_private_authorization(&execution, cx);
            }
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        self.open_hardware_spend_authorization_dialog(
            HardwareSpendAuthorizationCompletion::PrivateUnshieldSelfBroadcast {
                key,
                vault_password,
                authorization_limit,
                execution,
            },
            super::super::private_action::private_unshield_authorization_summary(&draft),
            window,
            cx,
        );
    }
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_spend_authorization_error_message(
    error: &HardwareSpendAuthorizationError,
) -> String {
    match error {
        HardwareSpendAuthorizationError::Hardware(error) => {
            format!("Hardware spend authorization failed: {error}")
        }
        HardwareSpendAuthorizationError::Vault(error) => format!("Vault error: {error}"),
        HardwareSpendAuthorizationError::Executor(error) => error.clone(),
    }
}

/// Fresh device approval of the hardware-derived wallet's synthetic seed entropy. The session
/// keeps the device's refreshed Trezor session.
#[cfg(feature = "hardware")]
pub(in crate::root) async fn hardware_synthetic_entropy(
    hardware_session: &mut HardwareProfileSession,
    descriptor: &HardwareDerivationDescriptor,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<TrezorPinMatrixProvider>,
) -> Result<wallet_ops::hardware::SyntheticRailgunEntropy, HardwareSpendAuthorizationError> {
    hardware_session.verify_descriptor(descriptor)?;
    Ok(match descriptor.device_kind {
        HardwareDeviceKind::Ledger => {
            let client = LedgerHardwareDerivationClient::connect().await?;
            let active = client.active_profile_session(&descriptor.path).await?;
            active.verify_descriptor(descriptor)?;
            let output = client.eip1024_shared_secret(&descriptor.path, true).await?;
            synthetic_entropy_from_hardware_output(descriptor, output)?
        }
        HardwareDeviceKind::Trezor => {
            let mut client = TrezorHardwareDerivationClient::connect_with_session(
                hardware_session.trezor_session_id.clone(),
            )?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let active = client.active_profile_session(&descriptor.path)?;
            active.verify_descriptor(descriptor)?;
            hardware_session
                .trezor_session_id
                .clone_from(&active.trezor_session_id);
            hardware_session.set_trezor_passphrase_mode(active.trezor_passphrase_mode());
            let output = client.cipher_key_value(descriptor)?;
            synthetic_entropy_from_hardware_output(descriptor, output)?
        }
    })
}

#[cfg(feature = "hardware")]
async fn derive_hardware_spend_authorization(
    store: Arc<DesktopVaultStore>,
    view_session: Arc<DesktopViewSession>,
    mut hardware_session: HardwareProfileSession,
    descriptor: HardwareDerivationDescriptor,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<TrezorPinMatrixProvider>,
    executor_requests: Option<(
        wallet_ops::HardwareExecutorAuthorizationRequest,
        Option<wallet_ops::HardwareExecutorAuthorizationRequest>,
    )>,
) -> HardwareSpendAuthorizationTaskOutput {
    if let Some((request, destination)) = &executor_requests {
        for request in std::iter::once(request).chain(destination) {
            request
                .ensure_active()
                .map_err(|error| HardwareSpendAuthorizationError::Executor(error.to_string()))?;
        }
    }
    let entropy = hardware_synthetic_entropy(
        &mut hardware_session,
        &descriptor,
        trezor_app_passphrase,
        trezor_pin_matrix_provider,
    )
    .await?;
    if let Some((request, destination)) = executor_requests {
        // A private Bridge swap's two requests are completed from this one device response.
        let (authorization, destination) = match destination {
            Some(destination) => {
                let (authorization, destination) = request
                    .complete_with_destination(destination, &descriptor, entropy.expose_secret())
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?;
                (
                    authorization,
                    Some(DesktopPrivateSpendAuthorization::HardwareExecutor(
                        Box::new(destination),
                    )),
                )
            }
            None => (
                request
                    .complete(&descriptor, entropy.expose_secret())
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?,
                None,
            ),
        };
        return Ok((
            DesktopPrivateSpendAuthorization::HardwareExecutor(Box::new(authorization)),
            destination,
            hardware_session,
        ));
    }
    let signer = store.hardware_railgun_spend_signer_from_entropy(
        view_session.as_ref(),
        &descriptor,
        entropy.expose_secret(),
    )?;
    Ok((
        DesktopPrivateSpendAuthorization::PreauthorizedSigner(signer),
        None,
        hardware_session,
    ))
}
