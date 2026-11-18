use super::*;

impl WalletRoot {
    pub(in crate::root::walletconnect) fn revalidate_walletconnect_pending_request(
        &self,
        request: &WalletConnectRequestUi,
        store: &DesktopVaultStore,
        view_session: &DesktopViewSession,
        now: u64,
    ) -> Result<
        (WalletConnectRequestUi, WalletConnectRequestRoute),
        WalletConnectSessionRequestFailure,
    > {
        if let Some(control) = &request.request_control {
            let failure = || WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "Dapp approval is no longer current".to_owned(),
            };
            control.ensure_current().map_err(|_| failure())?;
            let route = self
                .walletconnect
                .request_routes
                .get(&request.key)
                .cloned()
                .ok_or_else(failure)?;
            let DappRequestRoute::Gateway {
                handle,
                approval_id,
            } = &route
            else {
                return Err(failure());
            };
            let approvals = handle.approval_requests();
            let approvals = approvals.borrow();
            let ready = approvals
                .iter()
                .find(|ready| &ready.id == approval_id)
                .ok_or_else(failure)?;
            let authorization = ready.authorization.as_ref().ok_or_else(failure)?;
            let binding = request.binding.account.as_ref().ok_or_else(failure)?;
            let account = store
                .resolve_dapp_session_account(
                    view_session,
                    &binding.public_account_uuid,
                    &binding.public_account_scope,
                    binding.owning_private_wallet_uuid.as_deref(),
                )
                .map_err(|_| failure())?;
            let WalletConnectSessionAccountResolution::Usable(account) = account else {
                return Err(failure());
            };
            if account != authorization.account
                || request.item.account != Some(account.address)
                || request.item.chain_id != format!("eip155:{}", ready.chain_id)
            {
                return Err(failure());
            }
            wallet_ops::walletconnect::validate_dapp_request_account(
                &request.parsed,
                &account,
                ready.chain_id,
                walletconnect_namespace_account_support(&account, Some(view_session)),
            )
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: match error {
                    wallet_ops::walletconnect::DappRequestValidationError::UnsupportedMethod => {
                        WalletConnectRequestErrorKind::UnsupportedMethod
                    }
                    wallet_ops::walletconnect::DappRequestValidationError::AccountMismatch => {
                        WalletConnectRequestErrorKind::Unauthorized
                    }
                    _ => WalletConnectRequestErrorKind::MalformedParams,
                },
                message: "Dapp request is not supported by the current account".to_owned(),
            })?;
            self.ensure_walletconnect_chain_enabled(&request.item.chain_id)?;
            control.ensure_current().map_err(|_| failure())?;
            return Ok((request.clone(), route));
        }
        walletconnect_validate_pending_request_expiry(request.item.expiry_timestamp, now)?;
        let session_uuid = request
            .session_identity
            .walletconnect_session_id()
            .ok_or_else(|| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "Request does not belong to a WalletConnect session".to_owned(),
            })?;
        let session = store
            .load_walletconnect_session(view_session, session_uuid)
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: format!("Could not reload WalletConnect session: {error}"),
            })?;
        let binding = DappRequestBinding::from_walletconnect_session(&session);
        let account_binding =
            binding
                .account
                .as_ref()
                .ok_or_else(|| WalletConnectSessionRequestFailure {
                    kind: WalletConnectRequestErrorKind::Unauthorized,
                    message: "Request has no account authorization".to_owned(),
                })?;
        let resolution = store
            .resolve_dapp_session_account(
                view_session,
                &account_binding.public_account_uuid,
                &account_binding.public_account_scope,
                account_binding.owning_private_wallet_uuid.as_deref(),
            )
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: format!("Could not resolve WalletConnect Public account: {error}"),
            })?;
        let account_source = match &resolution {
            WalletConnectSessionAccountResolution::Usable(account) => account.source,
            WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                ..
            } => {
                return Err(WalletConnectSessionRequestFailure {
                    kind: WalletConnectRequestErrorKind::Unauthorized,
                    message:
                        "WalletConnect session is paused for a different selected Private wallet"
                            .to_owned(),
                });
            }
            WalletConnectSessionAccountResolution::InvalidPublicAccount => {
                return Err(WalletConnectSessionRequestFailure {
                    kind: WalletConnectRequestErrorKind::Unauthorized,
                    message: "WalletConnect session Public account is invalid".to_owned(),
                });
            }
        };
        let selected_account_support = match &resolution {
            WalletConnectSessionAccountResolution::Usable(account) => {
                walletconnect_namespace_account_support(account, Some(view_session))
            }
            WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                ..
            }
            | WalletConnectSessionAccountResolution::InvalidPublicAccount => {
                WalletConnectNamespaceAccountSupport::for_account_source(
                    PublicAccountSource::Derived,
                )
            }
        };
        let validation = validate_walletconnect_session_request_with_account_support(
            &session,
            &resolution,
            selected_account_support,
            &request.item.topic,
            request.item.id,
            &request.item.chain_id,
            request.parsed.clone(),
            None,
            now,
        )
        .map_err(|error| walletconnect_session_request_failure_from_error(&error))?;
        self.ensure_walletconnect_chain_enabled(&request.item.chain_id)?;
        if let WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } =
            &validation.request
        {
            self.ensure_walletconnect_chain_enabled(&format!("eip155:{chain_id}"))?;
        }
        let Some(mut item) = validation.approval_item else {
            return Err(WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: "WalletConnect request no longer requires approval".to_owned(),
            });
        };
        item.expiry_timestamp = request.item.expiry_timestamp;
        let route = WalletConnectRequestRoute::from_session(&session);
        Ok((
            WalletConnectRequestUi {
                request_control: None,
                rpc_reads: None,
                operation: request.operation.clone(),
                key: request.key.clone(),
                review_token: request.review_token,
                binding,
                session_identity: request.session_identity.clone(),
                parsed: request.parsed.clone(),
                item,
                account_source: Some(account_source),
            },
            route,
        ))
    }

    pub(in crate::root::walletconnect) fn publish_invalid_walletconnect_pending_request(
        &mut self,
        request_key: &str,
        failure: &WalletConnectSessionRequestFailure,
        response_sender: DappResponseSender,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let response = Err(DappRequestError {
            provider_failure: None,
            kind: failure.kind,
            message: failure.message.clone(),
        });
        let request_key = request_key.to_owned();
        self.walletconnect
            .request_actions
            .insert(request_key.clone());
        tracing::warn!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(&request_key),
            error = %failure.message,
            "walletconnect pending request failed revalidation"
        );
        let join = self.runtime.spawn(async move {
            response_sender
                .begin_approval()
                .await
                .map_err(|_| "Dapp approval is unavailable".to_owned())?;
            response_sender.send(response).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                root.walletconnect.remove_pending_request(&request_key);
                if root.walletconnect.request_dialog_key.as_deref() == Some(request_key.as_str()) {
                    root.clear_walletconnect_request_dialog_state(window, cx);
                    window.close_dialog(cx);
                }
                root.walletconnect.status = Some(Arc::from(
                    "WalletConnect request is no longer valid; error response published.",
                ));
                if let Ok(Err(error)) = result {
                    root.walletconnect.error = Some(Arc::from(format!(
                        "Request was removed locally, but relay error response failed: {error}"
                    )));
                }
                root.sync_platform_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn reject_walletconnect_request(
        &mut self,
        request_key: &str,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        let response_sender = match self.walletconnect_response_sender(&request, cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let response = Err(DappRequestError {
            provider_failure: None,
            kind: WalletConnectRequestErrorKind::UserRejected,
            message: "User rejected WalletConnect request".to_owned(),
        });
        let native = request.request_control.is_some();
        let request_key = request_key.to_owned();
        self.walletconnect
            .request_actions
            .insert(request_key.clone());
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(&request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            "rejecting walletconnect request"
        );
        let join = self
            .runtime
            .spawn(async move { response_sender.send(response).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                tracing::info!(
                    target: "wallet::root::walletconnect",
                    request_key = %walletconnect_request_key_log_label(&request_key),
                    relay_failed = matches!(&result, Ok(Err(_))),
                    relay_not_sent = matches!(&result, Ok(Err(error)) if !native && walletconnect_relay_request_was_not_sent(error)),
                    "walletconnect request rejection handled"
                );
                match result {
                    Ok(Ok(())) => {
                        root.walletconnect.remove_pending_request(&request_key);
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_walletconnect_request_dialog_state(window, cx);
                            window.close_dialog(cx);
                        }
                        root.walletconnect.status = Some(Arc::from("WalletConnect request rejected."));
                    }
                    Ok(Err(error)) if !native && walletconnect_relay_request_was_not_sent(&error) => {
                        root.walletconnect.status = Some(Arc::from(
                            "WalletConnect relay is reconnecting; rejection was not sent. The request remains pending so you can retry.",
                        ));
                        root.walletconnect.error = Some(Arc::from(error));
                    }
                    Ok(Err(error)) => {
                        root.walletconnect.remove_pending_request(&request_key);
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_walletconnect_request_dialog_state(window, cx);
                            window.close_dialog(cx);
                        }
                        root.walletconnect.status = Some(Arc::from("WalletConnect request rejected."));
                        root.walletconnect.error = Some(Arc::from(format!(
                            "Request was removed locally, but relay rejection failed: {error}"
                        )));
                    }
                    Err(error) => {
                        root.walletconnect.status = Some(Arc::from(
                            "WalletConnect rejection task failed. The request remains pending so you can retry.",
                        ));
                        root.walletconnect.error = Some(Arc::from(format!(
                            "WalletConnect rejection task failed: {error}"
                        )));
                    }
                }
                root.sync_platform_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
