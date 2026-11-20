#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn valid_spend_authorization_password(
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

    pub(super) fn cancel_spend_authorization(
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
    pub(super) fn report_stale_private_fee_review(
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

    pub(in crate::root) fn finish_spend_authorization(
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

    pub(in crate::root) fn clear_spend_authorization(&mut self, cx: &mut Context<'_, Self>) {
        if self.spend_authorization_cache.take().is_some() {
            cx.notify();
        }
    }

    pub(in crate::root) fn clear_protected_software_seed_session(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
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

    pub(super) fn current_spend_authorization_scope(&self) -> SpendAuthorizationScope {
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

    pub(super) fn desktop_spend_authorization(
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
}
