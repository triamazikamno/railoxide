use super::*;

impl PrivateSwapsView {
    pub(in crate::root) fn cancel_public_authorization(
        &mut self,
        command: &Arc<PublicSwapAuthorization>,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .public_authorization
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, command))
        {
            self.public_authorization = None;
            cx.notify();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::root) fn continue_authorized_public_swap(
        &mut self,
        command: &Arc<PublicSwapAuthorization>,
        private_authorization: DesktopPrivateSpendAuthorization,
        public_authorization: Option<DesktopPrivateSpendAuthorization>,
        trezor_app_passphrase: Option<zeroize::Zeroizing<String>>,
        trezor_pin_matrix_provider: Option<wallet_ops::HardwareTrezorPinMatrixProvider>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .public_authorization
            .take()
            .is_some_and(|current| Arc::ptr_eq(&current, command))
            || !self.session_is_current(cx)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| command.sessions_are_current(root.read(cx)))
        {
            return;
        }
        let Some(public_authorization) = public_authorization else {
            self.error = Some("Authorize the Public account before preparing this swap.".into());
            cx.notify();
            return;
        };
        if matches!(command.action, PublicSwapCommand::Swap { .. })
            && (!self
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.view == SwapDialogView::Form)
                || !self.form.as_ref().is_some_and(|form| {
                    form.quote_revision == command.revision
                        && form
                            .public
                            .as_ref()
                            .is_some_and(|public| public.account == command.source)
                }))
        {
            return;
        }
        if matches!(command.action, PublicSwapCommand::Swap { .. })
            && let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut())
        {
            public.operation = Some(command.operation);
            public.swap_use = Some(command.swap_use);
            public.review_change = None;
        }
        self.public_status = Some("Preparing the swap".into());
        let command = command.clone();
        self.start_public_job(
            command.clone(),
            async move {
                // Admitting the source can unlock an account or prompt its device; do it before
                // the destination acquires any claim or issues a signature.
                let signer = command
                    .owner
                    .authorize_public_swap_source(
                        &command.origin,
                        PublicSwapSource {
                            public_account_uuid: &command.source.public_account_uuid,
                            authorization: Some(&public_authorization),
                            trezor_app_passphrase,
                            trezor_pin_matrix_provider,
                        },
                        &command.client,
                    )
                    .await?;
                drop(public_authorization);
                let execution = PublicSwapExecution {
                    command,
                    private_authorization: Some(private_authorization),
                    signer,
                    signed: None,
                };
                prepare_public_execution(execution).await
            },
            window,
            cx,
        );
    }

    /// Whether the dialog still shows the swap of `command`: its reviewed form, or its detail
    /// once the swap is claimed. A recovery's command isn't tied to a view.
    pub(super) fn public_command_is_current(&self, command: &PublicSwapAuthorization) -> bool {
        if !matches!(command.action, PublicSwapCommand::Swap { .. }) {
            return true;
        }
        match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Form) => self.form.as_ref().is_some_and(|form| {
                form.quote_revision == command.revision
                    && form.network == Some(command.destination_session.chain_id)
                    && form.public.as_ref().is_some_and(|public| {
                        public.account == command.source
                            && public.operation == Some(command.operation)
                            && public.swap_use == Some(command.swap_use)
                    })
            }),
            Some(SwapDialogView::PublicDetail(shown)) => shown == command.identity(),
            _ => false,
        }
    }

    /// The Public account swap the wallet is working on: a job runs for it, or its execution
    /// is held between jobs.
    pub(in crate::root::private_swap) fn public_swap_at_work(
        &self,
    ) -> Option<crate::root::private_swap::model::SwapIdentity> {
        self.public_execution
            .as_ref()
            .map(|execution| execution.command.identity())
            .or(self.public_running)
    }

    /// Drop the Public account swap in preparation when the dialog leaves its form, or the
    /// detail that shows it at work. Its saved record continues it later.
    pub(in crate::root::private_swap) fn dismiss_public_preparation(&mut self) {
        let leaves = match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Form) => {
                self.form.as_ref().is_some_and(|form| form.public.is_some())
            }
            Some(SwapDialogView::PublicDetail(shown)) => self.public_swap_at_work() == Some(shown),
            _ => false,
        };
        if leaves {
            self.public_job = None;
            self.public_running = None;
            self.public_execution = None;
            self.public_authorization = None;
        }
    }

    pub(super) fn start_public_job(
        &mut self,
        command: Arc<PublicSwapAuthorization>,
        future: impl std::future::Future<Output = eyre::Result<PublicExecutionResult>> + Send + 'static,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let runtime = self.runtime.clone();
        self.public_running =
            matches!(command.action, PublicSwapCommand::Swap { .. }).then(|| command.identity());
        if self.public_running.is_some() {
            // An earlier failure doesn't stay beside the work that follows it.
            self.error = None;
        }
        self.public_job = Some(cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(future);
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.public_job = None;
                view.public_running = None;
                if !view.session_is_current(cx)
                    || !view.public_command_is_current(&command)
                    || !view
                        .root
                        .upgrade()
                        .is_some_and(|root| command.sessions_are_current(root.read(cx)))
                {
                    return;
                }
                match result {
                    Ok(Ok(PublicExecutionResult::Claimed(execution))) => {
                        // Debug UI fixture: a synthesized record stands in for the claim.
                        #[cfg(debug_assertions)]
                        view.stage_ui_fixture_swap(&command);
                        view.refresh_public_swap_records(cx);
                        // The swap has its record, so its detail shows the rest. The held
                        // execution marks the swap as at work, which keeps the switch from
                        // dropping it.
                        view.public_execution = Some(execution);
                        view.navigate(SwapDialogView::PublicDetail(command.identity()), window, cx);
                        view.prepare_claimed_public_swap(window, cx);
                    }
                    Ok(Ok(PublicExecutionResult::Waiting(execution))) => {
                        view.public_execution = Some(execution);
                    }
                    Ok(Ok(PublicExecutionResult::Ready(execution))) => {
                        // Only an order has signatures a device can't explain: its hook batch
                        // and the order itself. A deposit is a transaction the device decodes.
                        let decoded = matches!(
                            execution.command.source.source,
                            PublicAccountSource::HardwareDerived
                        ) && execution
                            .signed
                            .as_ref()
                            .is_some_and(|signed| signed.batch.is_some());
                        // Debug UI fixture: its staged signatures are reviewed without a device.
                        let decoded =
                            decoded || (ui_fixture::active() && execution.signed.is_some());
                        view.public_execution = Some(execution);
                        if decoded {
                            view.review_public_signatures(window, cx);
                        } else {
                            view.submit_public_signed(false, window, cx);
                        }
                    }
                    Ok(Ok(PublicExecutionResult::Changed { change, review })) => {
                        view.apply_public_review_change(&command, change, review, window, cx);
                    }
                    Ok(Ok(PublicExecutionResult::ProxyHolds { proxy, balance })) => {
                        let message = view.public_proxy_holds_text(&command, proxy, balance, cx);
                        view.reopen_public_swap_form(command.identity(), false, window, cx);
                        if let Some(form) = view.form.as_mut() {
                            form.error = Some(message.clone());
                            if let Some(public) = form.public.as_mut() {
                                public.proxy_holds = Some((message.clone(), proxy));
                            }
                        }
                        view.error = Some(message);
                    }
                    Ok(Ok(PublicExecutionResult::Finished(identity))) => {
                        view.public_execution = None;
                        view.refresh_public_swap_records(cx);
                        view.refresh_public_source_balance(identity, cx);
                        view.navigate(SwapDialogView::PublicDetail(identity), window, cx);
                    }
                    Ok(Err(error)) => {
                        // An open swap that trades the same token is named under the Sell
                        // card, where it keeps Review unavailable. Past the form, the detail
                        // shows the error and its record's actions take over.
                        let conflict = view
                            .form
                            .as_ref()
                            .and_then(|form| PublicSwapConflict::of(&error, &command, form));
                        if let Some(conflict) = conflict {
                            if let Some(public) =
                                view.form.as_mut().and_then(|form| form.public.as_mut())
                            {
                                public.conflict = Some(conflict);
                            }
                        } else {
                            view.error = Some(format!("{error:#}"));
                            if let Some(form) = view.form.as_mut() {
                                form.error.clone_from(&view.error);
                            }
                        }
                    }
                    Err(_) => {
                        view.error =
                            Some("The swap stopped. Check its status before trying again.".into());
                    }
                }
                view.refresh_public_swap_records(cx);
                // Debug UI fixture: a swap that stops at its review is in no record, and keeps
                // its form.
                if !ui_fixture::active()
                    && let Some(public) = view.form.as_mut().and_then(|form| form.public.as_mut())
                    && public.operation.is_some_and(|operation| {
                        !view.public_records.iter().any(|(_, record)| {
                            record.operation() == operation
                                && public
                                    .swap_use
                                    .is_some_and(|id| record.public_swap_use(id).is_some())
                        })
                    })
                {
                    public.operation = None;
                    public.swap_use = None;
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Start what follows the claim, which the swap's detail shows: the destination account's
    /// preparation and, for a new account, its setup.
    fn prepare_claimed_public_swap(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(execution) = self.public_execution.take() else {
            return;
        };
        let command = execution.command.clone();
        self.public_status = Some(match &command.action {
            // Debug UI fixture: its stand-in setup names no broadcaster.
            PublicSwapCommand::Swap {
                candidate,
                approval,
                ..
            } if candidate.is_some() || (ui_fixture::active() && approval.destination.setup) => {
                // Shown on the detail's setup step, which names the account and its network.
                "Sending the setup to a broadcaster".into()
            }
            _ => "Preparing the swap".into(),
        });
        self.start_public_job(command, prepare_public_destination(execution), window, cx);
    }

    pub(in crate::root::private_swap) fn continue_public_swap_after_setup(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_job.is_some() || self.public_authorization.is_some() {
            return;
        }
        let Some(execution) = self
            .public_execution
            .as_ref()
            .filter(|execution| execution.signed.is_none())
        else {
            if self.public_execution.is_none()
                && self.form.as_ref().is_some_and(|form| {
                    form.public
                        .as_ref()
                        .is_some_and(|public| public.review.is_none())
                        && form.quote_task.is_none()
                        && matches!(form.quote, QuoteState::Idle)
                })
            {
                self.schedule_public_quote(window, cx);
            }
            return;
        };
        if !self.public_command_is_current(&execution.command)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| execution.command.sessions_are_current(root.read(cx)))
        {
            self.public_execution = None;
            return;
        }
        let chain = execution.command.destination_session.chain_id;
        let Some(confirmed) = self
            .root
            .upgrade()
            .and_then(|root| root.read(cx).confirmed_block(chain))
        else {
            return;
        };
        // The job can't report that it got past the setup, so only an account already known to
        // be set up, as a continued swap's is, names what follows.
        let set_up = ui_fixture::setup_done()
            || matches!(
                self.public_setup_progress(execution.command.operation),
                Some(SwapSetupProgress::Done)
            );
        let execution = self.public_execution.take().expect("checked execution");
        // The job goes on to the approvals, which a hardware account's device signs.
        let approves = matches!(
            &execution.command.action,
            PublicSwapCommand::Swap { review, .. }
                if !review.gas_plan().approval_gas_limits.is_empty()
        );
        let device = self
            .public_device_hint(&execution.command, cx)
            .filter(|_| approves)
            .unwrap_or_default();
        self.public_status = Some(if set_up {
            format!(
                "{}{device}",
                Self::public_finishing_status(&execution.command)
            )
            .into()
        } else {
            format!(
                "Setting up the stealth account on {} · waiting for the setup to be confirmed{device}",
                network_name(chain)
            )
            .into()
        });
        self.start_public_job(
            execution.command.clone(),
            advance_public_execution(execution, confirmed),
            window,
            cx,
        );
    }
}
