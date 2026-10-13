//! One stealth account's setup sent again from its row in the swap's detail: a small dialog
//! with the form's broadcaster controls for that account's network, then the review of a fresh
//! fee estimate. The swap's approved terms are kept and aren't quoted again.

use super::*;
use crate::root::dialog_max_height;
use crate::root::private_broadcaster::SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH;
use crate::root::private_swap::model::{SwapIdentity, prepared_swap_use, swap_use_destination};
use crate::root::private_swap::progress::{fact_row, stealth_account};
use crate::root::private_swap::ui_fixture;

/// What a retry says once its setup is confirmed, included, stopped or already being sent.
const SETUP_RETRY_UNAVAILABLE: &str = "This setup can't be sent again now.";

/// The stealth account a retry sets up again, for the swap use whose approval it keeps.
struct RetryAccount {
    swap_use: SwapUseId,
    chain_id: u64,
    operation: ExecutorOperationId,
    /// `None` until the account's address is derived.
    shown: Option<SwapStepAccount>,
}

impl SetupRetryForm {
    /// Whether the retry's dialog is still among the window's dialogs.
    pub(super) fn is_open(&self) -> bool {
        self.focus
            .as_ref()
            .is_none_or(|focus| focus.upgrade().is_some())
    }
}

impl PrivateSwapsView {
    /// The account `side`'s setup of the swap `operation` is sent again for. `None` unless
    /// the swap is prepared with a saved approval and has no order.
    fn setup_retry_account(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
    ) -> Option<RetryAccount> {
        let record = self.record(operation)?;
        let swap_use =
            prepared_swap_use(record).filter(|swap_use| swap_use.approval().is_some())?;
        let swap_use = swap_use.id();
        match side {
            SetupSide::Origin => Some(RetryAccount {
                swap_use,
                chain_id: self.origin_chain_id,
                operation,
                shown: record.address().map(|address| SwapStepAccount {
                    index: Some(record.index()),
                    address,
                }),
            }),
            SetupSide::Destination => {
                let (delivery, account) = swap_use_destination(record, swap_use)?;
                let swap = SwapIdentity {
                    operation,
                    swap_use,
                };
                // A receiver that isn't bound yet takes the claimed account's address.
                let destination = self.claimed_destination_account(swap, delivery);
                let address = if delivery.receiver.is_zero() {
                    destination.and_then(ExecutorRecord::address)
                } else {
                    Some(delivery.receiver)
                };
                Some(RetryAccount {
                    swap_use,
                    chain_id: delivery.destination_chain,
                    operation: account,
                    shown: address.map(|address| SwapStepAccount {
                        index: destination.map(ExecutorRecord::index),
                        address,
                    }),
                })
            }
        }
    }

    /// Whether `side`'s setup of the swap `operation` may be sent again right now.
    fn setup_retry_allowed(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        cx: &App,
    ) -> bool {
        self.record(operation)
            .is_some_and(|record| self.setup_retry_availability(record, side, cx) == Some(Ok(())))
    }

    /// The retry's dialog is open with nothing in front of it.
    fn setup_retry_active(&self, window: &Window, cx: &App) -> bool {
        self.setup_retry
            .as_ref()
            .and_then(|retry| retry.focus.as_ref()?.upgrade())
            .is_some_and(|focus| focus.contains_focused(window, cx))
    }

    /// Open the retry of `side`'s setup of the swap `operation` in a dialog over the swap's
    /// detail. Its route starts from the fee token and filters the form would pick on the
    /// account's network, and follows broadcaster offers while the dialog is open.
    pub(in crate::root::private_swap) fn open_setup_retry(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy()
            || !self.session_is_current(cx)
            || self.form.is_some()
            || self.dialog.as_ref().map(|dialog| dialog.view)
                != Some(SwapDialogView::Detail(operation))
            || !self.setup_retry_allowed(operation, side, cx)
        {
            return;
        }
        let Some(account) = self.setup_retry_account(operation, side) else {
            return;
        };
        // Debug UI fixture: its synthesized destination account's network isn't loaded.
        if side == SetupSide::Destination
            && !ui_fixture::holds_swaps()
            && let Err(error) = self.ready_destination(account.chain_id, cx)
        {
            self.fail(operation, error);
            cx.notify();
            return;
        }
        self.setup_retry = Some(SetupRetryForm {
            operation,
            side,
            chain_id: account.chain_id,
            account: account.operation,
            shown: account.shown,
            route: SetupRoute::default(),
            focus: None,
            error: None,
        });
        let view = cx.entity().downgrade();
        let width =
            (window.viewport_size().width * 0.92).min(SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH);
        let max_height = dialog_max_height(window);
        window.open_dialog(cx, move |dialog, _, cx| {
            let (continue_view, close_view) = (view.clone(), view.clone());
            let content = view
                .update(cx, |view, cx| view.render_setup_retry(cx))
                .ok()
                .flatten();
            dialog
                .w(width)
                .max_h(max_height)
                .on_ok(move |_, window, cx| {
                    let _ = continue_view.update(cx, |view, cx| {
                        view.continue_setup_retry(window, cx);
                    });
                    false
                })
                .on_close(move |_, _, cx| {
                    let _ = close_view.update(cx, |view, cx| {
                        view.setup_retry = None;
                        cx.notify();
                    });
                })
                .children(content)
        });
        if let Some(retry) = self.setup_retry.as_mut() {
            retry.focus = window.focused(cx).as_ref().map(FocusHandle::downgrade);
        }
        self.start_setup_route_updates(window, cx);
        cx.notify();
    }

    /// Close the retry's dialog from one of its own actions, where it is the topmost dialog.
    fn close_setup_retry(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.setup_retry = None;
        window.close_dialog(cx);
        cx.notify();
    }

    /// The retry dialog's content: the account, the broadcaster controls of its network, the
    /// fee estimate of the route they chose, and its actions. `None` once the retry is dropped.
    fn render_setup_retry(&self, cx: &Context<'_, Self>) -> Option<gpui::Div> {
        let retry = self.setup_retry.as_ref()?;
        let (side, chain_id, route) = (retry.side, retry.chain_id, &retry.route);
        let busy = self.busy();
        let quoting = self.job.as_ref().is_some_and(|job| {
            job.operation == retry.operation && job.kind == SwapJobKind::SetupQuote
        });
        let unavailable = route.candidates.is_empty();
        let estimate = match &route.estimate {
            _ if unavailable => None,
            Some(estimate) => Some(
                fact_row(
                    "Estimated fee",
                    app_text(format!(
                        "≈ {}",
                        self.chain_amount(
                            chain_id,
                            estimate.broadcaster().token,
                            estimate.fee_amount(),
                            cx
                        )
                    )),
                )
                .debug_selector(|| "swap-setup-retry-estimate".into()),
            ),
            None => {
                let (status, problem) = self.setup_status(route, side, Some(chain_id), cx);
                Some(
                    app_muted_text(format!("Setup fee · {status}"))
                        .whitespace_normal()
                        .when(problem, |status| status.text_color(rgb(theme::DANGER)))
                        .debug_selector(|| "swap-setup-retry-estimate".into()),
                )
            }
        };
        let content = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "swap-setup-retry".into())
            .child(app_strong_text(format!(
                "Retry setup on {}",
                network_name(chain_id)
            )))
            .children(retry.shown.map(|account| {
                fact_row(
                    "Stealth account",
                    stealth_account("swap-setup-retry-account", account),
                )
            }))
            .child(setup_route_settings(
                cx.entity(),
                side,
                &SetupRouteSettings::of(route, busy),
            ))
            .children(estimate)
            .children(unavailable.then(|| {
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "swap-setup-retry-unavailable".into())
                    .child(
                        app_text(no_setup_broadcaster(chain_id))
                            .text_color(rgb(theme::WARNING))
                            .whitespace_normal(),
                    )
            }))
            .child(
                app_muted_text("The swap's terms stay as approved. You review the fee limit next.")
                    .whitespace_normal(),
            )
            .children(retry.error.clone().map(|error| {
                Alert::error("swap-setup-retry-error", error)
                    .small()
                    .min_w_0()
            }))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("swap-setup-retry-cancel", "Cancel")
                            .debug_selector(|| "swap-setup-retry-cancel".into())
                            .small()
                            .flex_none()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_setup_retry(window, cx);
                            })),
                    )
                    .child(
                        app_button("swap-setup-retry-continue", "Continue")
                            .debug_selector(|| "swap-setup-retry-continue".into())
                            .primary()
                            .small()
                            .flex_none()
                            .loading(quoting)
                            .disabled(busy || unavailable)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.continue_setup_retry(window, cx);
                            })),
                    ),
            );
        Some(content)
    }

    /// Estimate the retry's setup fee for the broadcaster its route chose, then open its
    /// review. Nothing is estimated or signed without a broadcaster.
    pub(super) fn continue_setup_retry(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        // A retry that can't continue connects to no broadcaster network.
        if self.busy()
            || self
                .setup_retry
                .as_ref()
                .is_none_or(|retry| retry.route.candidates.is_empty())
        {
            return;
        }
        let waku = self.broadcaster_network(cx);
        self.quote_setup_retry(waku, window, cx);
    }

    /// [`Self::continue_setup_retry`] with `waku`, the broadcaster network's client.
    pub(super) fn quote_setup_retry(
        &mut self,
        waku: Option<Arc<WakuDeliveryClient>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(retry) = self.setup_retry.as_ref() else {
            return;
        };
        let (operation, side, chain_id, account) =
            (retry.operation, retry.side, retry.chain_id, retry.account);
        // The broadcaster the estimate on screen is for, or the route's choice without one.
        let candidate = retry
            .route
            .offered_estimate()
            .map(|estimate| estimate.broadcaster().clone())
            .or_else(|| self.setup_route_candidate(&retry.route, cx));
        let Some(candidate) = candidate else {
            return;
        };
        let ready = match side {
            _ if !self.setup_retry_allowed(operation, side, cx) => {
                Err(SETUP_RETRY_UNAVAILABLE.to_owned())
            }
            SetupSide::Origin => self
                .private_session()
                .cloned()
                .zip(self.private_owner().cloned())
                .ok_or_else(|| "The wallet session ended.".to_owned()),
            SetupSide::Destination => self.ready_destination(chain_id, cx),
        };
        let ready = ready.and_then(|ready| {
            waku.map(|waku| (ready, waku)).ok_or_else(|| {
                "Wait for the broadcaster network connection, then try again.".to_owned()
            })
        });
        let ((session, owner), waku) = match ready {
            Ok(ready) => ready,
            Err(error) => {
                if let Some(retry) = self.setup_retry.as_mut() {
                    retry.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        if let Some(retry) = self.setup_retry.as_mut() {
            retry.error = None;
        }
        self.start_job(
            operation,
            SwapJobKind::SetupQuote,
            // An estimate that fails is told in the retry's dialog, which stays open.
            setup_job_result(async move {
                Box::pin(owner.estimate_swap_setup_fee(&session, Some(account), candidate)).await
            }),
            move |this, estimate, window, cx| {
                this.review_setup_retry(operation, side, estimate, waku, window, cx);
            },
            window,
            cx,
        );
    }

    /// Open the review of the retry with its fresh fee `estimate`, in place of the retry's
    /// dialog. A retry that was closed or covered while its fee was estimated opens none.
    fn review_setup_retry(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        estimate: eyre::Result<ExecutorRecoveryFeeEstimate>,
        waku: Arc<WakuDeliveryClient>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .setup_retry
            .as_ref()
            .is_some_and(|retry| retry.operation == operation && retry.side == side)
            || !self.setup_retry_active(window, cx)
        {
            return;
        }
        let approval = estimate
            .map_err(|error| format!("{error:#}"))
            .and_then(|estimate| self.setup_retry_approval(operation, side, &estimate, waku, cx));
        match approval {
            Ok(retry) => {
                let summary = self.setup_retry_summary(&retry, cx);
                self.close_setup_retry(window, cx);
                self.request_authorization(
                    SwapAction::SetupRetry(Box::new(retry)),
                    summary,
                    window,
                    cx,
                );
            }
            Err(error) => {
                if let Some(retry) = self.setup_retry.as_mut() {
                    retry.error = Some(error);
                }
                cx.notify();
            }
        }
    }

    /// The retry of `side`'s setup of the swap `operation` as its review approves it, with
    /// the fresh fee `estimate`. The fee limit approved with the swap binds the setup, so a
    /// higher one comes with the swap's approval to save. The approval's pair, amount, minimum
    /// and delivery are the saved ones.
    pub(super) fn setup_retry_approval(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        estimate: &ExecutorRecoveryFeeEstimate,
        waku: Arc<WakuDeliveryClient>,
        cx: &App,
    ) -> Result<SetupRetry, String> {
        let unavailable = || SETUP_RETRY_UNAVAILABLE.to_owned();
        if !self.setup_retry_allowed(operation, side, cx) {
            return Err(unavailable());
        }
        let account = self
            .setup_retry_account(operation, side)
            .ok_or_else(unavailable)?;
        let saved = self
            .record(operation)
            .and_then(|record| record.swap_use(account.swap_use)?.approval())
            .ok_or_else(unavailable)?
            .clone();
        let maximum = default_public_broadcaster_fee_limit(estimate.fee_amount());
        let approved_fee = match side {
            SetupSide::Origin => saved.bounds.source_setup_fee,
            SetupSide::Destination => saved.bounds.destination_setup_fee,
        };
        let raised = approved_fee.is_none_or(|approved| maximum > approved);
        Ok(SetupRetry {
            operation,
            side,
            swap_use: account.swap_use,
            chain_id: account.chain_id,
            account: account.operation,
            candidate: estimate.broadcaster().clone(),
            maximum_private_fee: maximum,
            waku,
            approved_fee,
            approval: raised.then(|| {
                let mut approval = saved;
                match side {
                    SetupSide::Origin => approval.bounds.source_setup_fee = Some(maximum),
                    SetupSide::Destination => {
                        approval.bounds.destination_setup_fee = Some(maximum);
                    }
                }
                approval
            }),
        })
    }

    /// The other account of the swap `retry` belongs to, when the swap sets that one up too:
    /// its network, and whether its setup is confirmed.
    fn setup_retry_other(&self, retry: &SetupRetry) -> Option<(String, bool)> {
        let record = self.record(retry.operation)?;
        let (other, network) = match retry.side {
            SetupSide::Origin => {
                let (delivery, _) = swap_use_destination(record, retry.swap_use)?;
                (
                    SetupSide::Destination,
                    network_name(delivery.destination_chain),
                )
            }
            SetupSide::Destination => (SetupSide::Origin, self.chain_label()),
        };
        let progress = self.prepared_setup_progress(record, other)?;
        Some((network, progress == SwapSetupProgress::Done))
    }

    /// The review of a setup sent again by itself: its fee limit, which a limit above the
    /// approved one is named with, and the stealth account it sets up.
    pub(super) fn setup_retry_summary(
        &self,
        retry: &SetupRetry,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let (chain_id, candidate, maximum) =
            (retry.chain_id, &retry.candidate, retry.maximum_private_fee);
        let network = network_name(chain_id);
        let fee = SetupFee {
            chain_id,
            token: candidate.token,
            maximum,
            broadcaster: broadcaster_candidate_label(candidate),
        };
        let amount = |amount: U256| {
            let root = self.root.upgrade();
            format_token_amount_ceiling_for_display(
                chain_id,
                candidate.token,
                amount,
                root.as_ref()
                    .map(|root| &root.read(cx).effective_token_registry),
            )
        };
        let mut rows = vec![
            self.setup_fee_row(std::slice::from_ref(&fee), cx)
                .with_amount_change(retry.approved_fee, maximum, true, amount),
        ];
        let account = self
            .setup_retry_account(retry.operation, retry.side)
            .and_then(|account| account.shown);
        if let Some(account) = account {
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Stealth account",
                    account.address.to_checksum(None),
                )
                .with_shortened_copyable(),
            );
        }
        let warnings = match retry.approved_fee.filter(|approved| maximum > *approved) {
            Some(approved) => vec![Arc::from(format!(
                "The setup fee on {network} is above the {} you approved. Approving this review saves the new limit.",
                amount(approved)
            ))],
            None => Vec::new(),
        };
        let subtitle = match self.setup_retry_other(retry) {
            Some((other, true)) => format!(
                "The setup on {other} is confirmed and isn't paid again. The swap's order is placed once this setup is confirmed too."
            ),
            Some((other, false)) => format!(
                "Only the setup on {network} is sent again. The setup on {other} isn't confirmed yet, and the swap's order is placed once both are."
            ),
            None => "Only the setup is sent again. The swap's terms stay as approved, and its order is placed once the setup is confirmed.".to_owned(),
        };
        SpendAuthorizationSummary::new("Set up stealth account", subtitle, rows)
            .with_title_network(chain_id)
            .with_compact_rows()
            .with_confirm_label("Create stealth account")
            .with_warnings(warnings)
            .requiring_explicit_review()
    }

    /// Hand the approved setup to its broadcaster through its account's own network: the
    /// swap's session and owner, or the destination network's. The other account's setup
    /// stays as it is, and the order follows once every setup is confirmed.
    pub(in crate::root::private_swap) fn submit_setup_retry(
        &mut self,
        retry: SetupRetry,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (operation, side) = (retry.operation, retry.side);
        let Some(owner) = self.private_owner().cloned() else {
            return;
        };
        let (session, account_owner) = match side {
            SetupSide::Origin => {
                let Some(session) = self.private_session().cloned() else {
                    return;
                };
                (session, Arc::clone(&owner))
            }
            SetupSide::Destination => match self.ready_destination(retry.chain_id, cx) {
                Ok(ready) => ready,
                Err(error) => {
                    self.fail(operation, error);
                    cx.notify();
                    return;
                }
            },
        };
        // A private Bridge swap's problem names the network of the setup it is about.
        let network = self
            .record(operation)
            .is_some_and(|record| swap_use_destination(record, retry.swap_use).is_some())
            .then(|| network_name(retry.chain_id));
        // The new attempt is observed afresh.
        let tracking = self.tracking.entry(operation).or_default();
        match side {
            SetupSide::Origin => {
                tracking.setup = None;
                tracking.cursor = None;
            }
            SetupSide::Destination => tracking.destination_setup = None,
        }
        tracking.error = None;
        tracking.auto_place = true;
        // Only this account's stages start over. The other account's stay as they ended.
        let progress = self.start_setup_progress(operation, side, cx);
        self.start_job(
            operation,
            SwapJobKind::Setup,
            setup_job_result(async move {
                let prepared = Box::pin(account_owner.resume_swap_setup(
                    retry.account,
                    retry.candidate,
                    &authorization,
                ))
                .await?;
                // The swap's approval binds the accounts as they are derived by now, which a
                // preparation interrupted before their derivation left unbound: the swap's
                // own, and the destination account as its delivery's receiver. A reviewed fee
                // above the approved limit is saved with them, before the setup is sent.
                owner.bind_swap_use_accounts(operation, retry.swap_use, retry.approval)?;
                let outcome = Box::pin(account_owner.submit_swap_setup(
                    &prepared,
                    SwapSetupRequest {
                        maximum_private_fee: retry.maximum_private_fee,
                        session,
                        authorization,
                        waku: retry.waku,
                        verify_proof: true,
                        progress_tx: Some(progress),
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                    },
                ))
                .await?;
                Ok(outcome.result)
            }),
            move |this, result, window, cx| {
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        this.fail_setup_job(operation, &error, cx);
                        return;
                    }
                };
                // A problem the swap's detail shows on the setup's stages isn't told again as
                // the swap's error.
                let shown = this.finish_setup_progress(operation, side, Some(Ok(&result)))
                    && this.shows_setup_progress(operation, side, cx);
                this.tracking.entry(operation).or_default().error =
                    broadcaster_result_problem(&result, "setup")
                        .filter(|_| !shown)
                        .map(|problem| match &network {
                            Some(network) => format!("{network}: {problem}"),
                            None => problem,
                        });
                if !window.has_active_dialog(cx) {
                    this.show_detail(operation, window, cx);
                }
                cx.notify();
            },
            window,
            cx,
        );
    }
}

/// What a retry says while no broadcaster on `chain_id` can be paid for its setup.
fn no_setup_broadcaster(chain_id: u64) -> String {
    format!(
        "No compatible broadcaster on {0} accepts a private fee token you hold there. Add private funds on {0}, or try again later.",
        network_name(chain_id)
    )
}
