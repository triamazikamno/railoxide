#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn continue_authorized_spend(
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
    pub(super) fn continue_authorized_spend_with_destination(
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
