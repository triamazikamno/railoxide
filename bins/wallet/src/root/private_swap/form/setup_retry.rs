//! One stealth account's setup sent again from its row in the swap's detail: a small dialog
//! with the form's broadcaster controls for that account's network, then the review of a fresh
//! fee estimate. The swap's approved terms are kept and aren't quoted again. For an account
//! that was retired because it had previous activity, the same dialog and review set up a new
//! account in its place.

use super::*;
use crate::root::dialog_max_height;
use crate::root::private_broadcaster::SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH;
use crate::root::private_swap::model::{
    SwapIdentity, order_setup_use, prepared_swap_use, swap_use_destination,
};
use crate::root::private_swap::progress::{fact_row, stealth_account};
use crate::root::private_swap::sets_up_destination;
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

    /// The retry dialog's own focus, while the dialog is open.
    pub(in crate::root::private_swap) fn focus(&self) -> Option<FocusHandle> {
        self.focus.as_ref()?.upgrade()
    }
}

/// The side of a swap use that `side`'s setup belongs to.
const fn swap_use_side(side: SetupSide) -> SwapUseSide {
    match side {
        SetupSide::Origin => SwapUseSide::Source,
        SetupSide::Destination => SwapUseSide::Destination,
    }
}

/// Drop what `approval` binds of the account on `side`, as replacing that account drops it from
/// the saved approval: its address and, for the destination, the delivery's receiver. The
/// approval binds the replacement once that account is derived.
fn unbind_replaced_account(approval: &mut SwapApproval, side: SetupSide) {
    match side {
        SetupSide::Origin => {
            if let Some(accounts) = approval.accounts.as_mut() {
                accounts.source.address = None;
            }
        }
        SetupSide::Destination => {
            if let Some(destination) = approval
                .accounts
                .as_mut()
                .and_then(|accounts| accounts.destination.as_mut())
            {
                destination.address = None;
            }
            if let Some(delivery) = approval.delivery.private_bridge() {
                approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
                    receiver: Address::ZERO,
                    ..delivery
                });
            }
        }
    }
}

impl PrivateSwapsView {
    /// Whether the retry of `side`'s setup of the swap `operation` sets up a new account: the
    /// account there was retired because it had previous activity, and the swap has a saved
    /// approval and no order. The swap then goes on with a newly reserved account under that
    /// approval. A swap without one is reviewed again in its form. The swap's own account is
    /// replaced first, and its destination account only once the swap's own has an address,
    /// which saving the approval with the new account needs.
    pub(in crate::root::private_swap) fn retries_with_new_account(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
    ) -> bool {
        let Some(record) = self
            .record(operation)
            .filter(|record| !record.is_swap_setup_stopped())
        else {
            return false;
        };
        let Some(swap_use) =
            prepared_swap_use(record).filter(|swap_use| swap_use.approval().is_some())
        else {
            return false;
        };
        match side {
            SetupSide::Origin => swap_use.is_fresh() && record.is_retired(),
            SetupSide::Destination => {
                let swap = SwapIdentity {
                    operation,
                    swap_use: swap_use.id(),
                };
                !record.is_retired()
                    && record.address().is_some()
                    && sets_up_destination(swap_use)
                    && swap_use_destination(record, swap.swap_use)
                        .and_then(|(delivery, _)| self.claimed_destination_account(swap, delivery))
                        .is_some_and(ExecutorRecord::is_retired)
            }
        }
    }

    /// Open what continues the swap `operation` from its detail's footer: the retry with a
    /// new account for a swap that stopped at its own retired account and has a saved
    /// approval, and the swap's form for any other.
    pub(in crate::root::private_swap) fn resume_swap(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.retries_with_new_account(operation, SetupSide::Origin) {
            self.open_setup_retry(operation, SetupSide::Origin, window, cx);
        } else {
            self.open_existing_form(operation, window, cx);
        }
    }

    /// The account `side`'s setup of the swap `operation` is sent again for. `None` unless
    /// the swap is prepared with a saved approval and has no order, or, for the swap's own
    /// account, its orders were placed on its setup while the record leaves that setup
    /// unresolved.
    fn setup_retry_account(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
    ) -> Option<RetryAccount> {
        let record = self.record(operation)?;
        let swap_use = prepared_swap_use(record)
            .filter(|swap_use| swap_use.approval().is_some())
            .or_else(|| {
                (side == SetupSide::Origin)
                    .then(|| order_setup_use(record))
                    .flatten()
            })?;
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

    /// Whether `side`'s setup of the swap `operation` may be sent again right now, or a new
    /// account set up in place of its retired one.
    fn setup_retry_allowed(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        cx: &App,
    ) -> bool {
        self.retries_with_new_account(operation, side)
            || self.record(operation).is_some_and(|record| {
                self.setup_retry_availability(record, side, cx) == Some(Ok(()))
            })
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
    /// account's network, and follows broadcaster offers while the dialog is open. A retired
    /// account's retry names no account: its new one is reserved later.
    pub(in crate::root::private_swap) fn open_setup_retry(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx)
            || self.form.is_some()
            || self.dialog.as_ref().map(|dialog| dialog.view)
                != Some(SwapDialogView::Detail(operation))
            || !self.setup_retry_allowed(operation, side, cx)
        {
            return;
        }
        // A setup that waited too long for a broadcaster's answer is sent again in place of
        // that wait.
        if self.setup_wait_outlasted(operation, side) {
            self.stop_setup_wait(operation);
        }
        if self.busy() {
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
        let new_account = self.retries_with_new_account(operation, side);
        let route = SetupRoute::default();
        // Debug UI fixture: its stand-in estimate is the one the dialog shows and reviews.
        #[cfg(debug_assertions)]
        let route = SetupRoute {
            estimate: self.ui_fixture_setup_estimate(cx),
            ..route
        };
        self.setup_retry = Some(SetupRetryForm {
            operation,
            side,
            chain_id: account.chain_id,
            account: account.operation,
            shown: account.shown.filter(|_| !new_account),
            route,
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
        let new_account = self.retries_with_new_account(retry.operation, side);
        let unavailable = route.candidates.is_empty();
        // Debug UI fixture: its stand-in estimate needs no broadcaster.
        #[cfg(debug_assertions)]
        let unavailable = unavailable && !Self::holds_ui_fixture_estimate(route);
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
            .child(app_strong_text(if new_account {
                format!("Set up a new account on {}", network_name(chain_id))
            } else {
                format!("Retry setup on {}", network_name(chain_id))
            }))
            .children(retry.shown.map(|account| {
                fact_row(
                    "Stealth account",
                    stealth_account("swap-setup-retry-account", account),
                )
            }))
            .children(new_account.then(|| {
                fact_row("Stealth account", app_text(NEW_ACCOUNT))
                    .debug_selector(|| "swap-setup-retry-new-account".into())
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
                app_muted_text(if new_account {
                    "The account reserved for this swap has previous activity, so a new one takes its place. The swap's terms stay as approved. You review the fee limit next."
                } else {
                    "The swap's terms stay as approved. You review the fee limit next."
                })
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
        // Debug UI fixture: its stand-in estimate is reviewed as it is.
        #[cfg(debug_assertions)]
        if self.review_ui_fixture_setup_estimate(window, cx) {
            return;
        }
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
        waku: Option<PublicBroadcasterWaku>,
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
        // A new account's fee is estimated as a first setup's is, for no reserved account.
        let account = (!self.retries_with_new_account(operation, side)).then_some(account);
        let Some(http) = self.http(cx) else {
            return;
        };
        self.start_job(
            operation,
            SwapJobKind::SetupQuote,
            // An estimate that fails is told in the retry's dialog, which stays open.
            setup_job_result(async move {
                let client = http.operation_http_client().await?;
                Box::pin(owner.estimate_swap_setup_fee(&session, account, candidate, &client)).await
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
        waku: PublicBroadcasterWaku,
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
            .and_then(|estimate| {
                let operation = self.reserve_reviewed_replacement(operation, side, cx)?;
                self.setup_retry_approval(operation, side, &estimate, waku, cx)
            });
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

    /// For a hardware wallet whose retry sets up a new account, reserve that account before
    /// the review opens: the device approves one account's action, so the review has to name
    /// the replacement. The swap moves to it at once, and the retry goes on as an ordinary one
    /// for it, also when its review is cancelled. A software wallet's replacement is reserved
    /// by the retry's job, once its review is approved. The swap's own account's operation
    /// afterwards is returned.
    pub(super) fn reserve_reviewed_replacement(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        cx: &mut Context<'_, Self>,
    ) -> Result<ExecutorOperationId, String> {
        let hardware = self
            .root
            .upgrade()
            .is_some_and(|root| root.read(cx).selected_wallet_source().is_hardware_derived());
        // Debug UI fixture: no owner holds its synthesized record.
        if !hardware || ui_fixture::holds_swaps() || !self.retries_with_new_account(operation, side)
        {
            return Ok(operation);
        }
        let ended = || "The wallet session ended.".to_owned();
        let swap_use = self
            .setup_retry_account(operation, side)
            .ok_or_else(|| SETUP_RETRY_UNAVAILABLE.to_owned())?
            .swap_use;
        let owner = self.private_owner().cloned().ok_or_else(ended)?;
        let destination = self.swap_destination_owner(operation, swap_use, cx);
        let swap = owner
            .replace_swap_account(
                destination.as_ref().map(|(owner, _)| owner.as_ref()),
                operation,
                swap_use,
                swap_use_side(side),
            )
            .map_err(|error| format!("{error:#}"))?;
        self.follow_replaced_account(
            ReplacedSwapAccount {
                from: operation,
                swap,
            },
            cx,
        );
        if let Some(account) = self.setup_retry_account(swap.operation, side)
            && let Some(retry) = self.setup_retry.as_mut()
        {
            retry.operation = swap.operation;
            retry.account = account.operation;
            retry.shown = account.shown;
        }
        Ok(swap.operation)
    }

    /// A private Bridge swap's destination network's owner, when that network is loaded, with
    /// the destination account the use `swap_use` of the swap `operation` names there.
    fn swap_destination_owner(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        cx: &App,
    ) -> Option<(Arc<ExecutorOwner>, ExecutorOperationId)> {
        let (delivery, account) = swap_use_destination(self.record(operation)?, swap_use)?;
        let (_, owner) = self.destination_owner(delivery.destination_chain, cx)?;
        Some((owner, account))
    }

    /// Debug UI fixture: whether `route` holds the fixture's stand-in estimate, which no
    /// broadcaster offers.
    #[cfg(debug_assertions)]
    fn holds_ui_fixture_estimate(route: &SetupRoute) -> bool {
        ui_fixture::holds_swaps() && route.estimate.is_some()
    }

    /// Debug UI fixture: open the review of the open retry with the stand-in estimate its
    /// route holds, in place of estimating a fee. `false` when the route holds none.
    #[cfg(debug_assertions)]
    fn review_ui_fixture_setup_estimate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some((operation, side, estimate)) = self
            .setup_retry
            .as_ref()
            .filter(|retry| Self::holds_ui_fixture_estimate(&retry.route))
            .and_then(|retry| Some((retry.operation, retry.side, retry.route.estimate.clone()?)))
        else {
            return false;
        };
        match self.broadcaster_network(cx) {
            Some(waku) => {
                self.review_setup_retry(operation, side, Ok(estimate), waku, window, cx);
            }
            None => {
                tracing::warn!("the debug UI fixture's review needs the broadcaster network");
            }
        }
        true
    }

    /// Debug UI fixture: in a `setup:fee-token` mode, open the review of the synthesized swap's
    /// own setup sent again with `estimate`, the fixture's stand-in for a broadcaster's, over
    /// the swap's detail.
    #[cfg(debug_assertions)]
    pub(in crate::root::private_swap) fn open_ui_fixture_setup_retry_review(
        &mut self,
        operation: ExecutorOperationId,
        estimate: &ExecutorRecoveryFeeEstimate,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(waku) = self.broadcaster_network(cx) else {
            tracing::warn!("the debug UI fixture's review needs the broadcaster network");
            return;
        };
        match self.setup_retry_approval(operation, SetupSide::Origin, estimate, waku, cx) {
            Ok(retry) => {
                let summary = self.setup_retry_summary(&retry, cx);
                self.request_authorization(
                    SwapAction::SetupRetry(Box::new(retry)),
                    summary,
                    window,
                    cx,
                );
            }
            Err(error) => {
                tracing::warn!(%error, "the debug UI fixture couldn't open its setup review");
            }
        }
    }

    /// The retry of `side`'s setup of the swap `operation` as its review approves it, with
    /// the fresh fee `estimate`. The fee token and limit approved with the swap bind the setup,
    /// so a higher limit or another token comes with the swap's approval to save, for this
    /// account only. The approval's pair, amount, minimum and delivery are the saved ones. A
    /// retry that sets up a new account in place of a retired one carries the approval without
    /// the retired account, for the new one to be bound.
    pub(super) fn setup_retry_approval(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        estimate: &ExecutorRecoveryFeeEstimate,
        waku: PublicBroadcasterWaku,
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
        let reviewed = SwapSetupFeeLimit {
            token: estimate.broadcaster().token,
            amount: maximum,
        };
        let saves =
            setup_fee_limit_change(approved_fee, reviewed) != Some(SetupFeeLimitChange::Within);
        let replaces = self.retries_with_new_account(operation, side);
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
            approval: saves.then(|| {
                let mut approval = saved;
                match side {
                    SetupSide::Origin => approval.bounds.source_setup_fee = Some(reviewed),
                    SetupSide::Destination => {
                        approval.bounds.destination_setup_fee = Some(reviewed);
                    }
                }
                if replaces {
                    unbind_replaced_account(&mut approval, side);
                }
                approval
            }),
            replaces,
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

    /// The review of a setup sent again by itself: its fee limit, and the stealth account it
    /// sets up, or "New account" for one that isn't derived yet or replaces a retired one. A
    /// limit above the approved one is named with it and with the change in the
    /// fee token and in USD. Another fee token is named with both limits, each in its own
    /// token, and with the change in USD alone, which is left out without a cached price for
    /// both tokens.
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
        let reviewed = SwapSetupFeeLimit {
            token: candidate.token,
            amount: maximum,
        };
        let amount = |limit: SwapSetupFeeLimit| {
            let root = self.root.upgrade();
            format_token_amount_ceiling_for_display(
                chain_id,
                limit.token,
                limit.amount,
                root.as_ref()
                    .map(|root| &root.read(cx).effective_token_registry),
            )
        };
        let change = setup_fee_limit_change(retry.approved_fee, reviewed);
        // Both limits in USD at the prices cached now, when both tokens have one.
        let values = retry.approved_fee.and_then(|approved| {
            let value = |limit: SwapSetupFeeLimit| {
                self.network_usd_micro_value(chain_id, limit.token, limit.amount, cx)
            };
            Some((value(approved)?, value(reviewed)?))
        });
        // Limits in different tokens have no difference in token units.
        let amounts = retry
            .approved_fee
            .filter(|_| change != Some(SetupFeeLimitChange::AnotherToken))
            .map(|approved| (approved.amount, maximum));
        let mut rows = vec![
            self.setup_fee_row(std::slice::from_ref(&fee), cx)
                .with_priced_amount_change(
                    amounts,
                    values,
                    true,
                    |difference| {
                        amount(SwapSetupFeeLimit {
                            token: candidate.token,
                            amount: difference,
                        })
                    },
                    railgun_ui::format_usd_micro_value,
                ),
        ];
        let account = self
            .setup_retry_account(retry.operation, retry.side)
            .filter(|_| !retry.replaces)
            .and_then(|account| account.shown);
        rows.push(match account {
            Some(account) => SpendAuthorizationSummaryRow::new(
                "Stealth account",
                account.address.to_checksum(None),
            )
            .with_shortened_copyable(),
            None => SpendAuthorizationSummaryRow::new("Stealth account", NEW_ACCOUNT),
        });
        let warnings = match retry.approved_fee.zip(change) {
            Some((approved, SetupFeeLimitChange::Higher)) => vec![Arc::from(format!(
                "The setup fee on {network} is above the {} you approved. Approving this review saves the new limit.",
                amount(approved)
            ))],
            Some((approved, SetupFeeLimitChange::AnotherToken)) => {
                let symbol = |token| self.network_token_symbol(chain_id, token, cx);
                let (approved_value, reviewed_value) = match values {
                    Some((before, now)) => (
                        format!(" ({})", railgun_ui::format_usd_micro_value(before)),
                        format!(" ({})", railgun_ui::format_usd_micro_value(now)),
                    ),
                    None => (String::new(), String::new()),
                };
                vec![Arc::from(format!(
                    "The fee token changed from {} to {}. You approved up to {}{approved_value}; this setup pays up to {}{reviewed_value}. Approving this review saves the new limit.",
                    symbol(approved.token),
                    symbol(reviewed.token),
                    amount(approved),
                    amount(reviewed),
                ))]
            }
            _ => Vec::new(),
        };
        let subtitle = match (retry.replaces, self.setup_retry_other(retry)) {
            (false, Some((other, true))) => format!(
                "The setup on {other} is confirmed and isn't paid again. The swap's order is placed once this setup is confirmed too."
            ),
            (false, Some((other, false))) => format!(
                "Only the setup on {network} is sent again. The setup on {other} isn't confirmed yet, and the swap's order is placed once both are."
            ),
            (false, None) => "Only the setup is sent again. The swap's terms stay as approved, and its order is placed once the setup is confirmed.".to_owned(),
            (true, Some((other, true))) => format!(
                "A new stealth account on {network} takes the place of the one with previous activity. The setup on {other} is confirmed and isn't paid again."
            ),
            (true, Some((other, false))) => format!(
                "A new stealth account on {network} takes the place of the one with previous activity. The setup on {other} isn't confirmed yet, and the swap's order is placed once both are."
            ),
            (true, None) => "A new stealth account takes the place of the one with previous activity. The swap's terms stay as approved, and its order is placed once the setup is confirmed.".to_owned(),
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
    ///
    /// A retry that sets up a new account first replaces the retired one, and the view
    /// follows the swap to its new account. An account this job prepares and finds used is
    /// replaced as a setup job replaces it, through [`SwapAccountReplacements`]: within its
    /// limit with a vault password, and never with a hardware wallet's authorization.
    pub(in crate::root::private_swap) fn submit_setup_retry(
        &mut self,
        retry: SetupRetry,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (operation, side) = (retry.operation, retry.side);
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
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
        // A private Bridge swap's destination account, with its network's owner.
        let destination = match side {
            SetupSide::Origin => self.swap_destination_owner(operation, retry.swap_use, cx),
            SetupSide::Destination => Some((account_owner, retry.account)),
        };
        // A private Bridge swap's problem names the network of the setup it is about.
        let network = self
            .record(operation)
            .is_some_and(|record| swap_use_destination(record, retry.swap_use).is_some())
            .then(|| network_name(retry.chain_id));
        // An order placed on the setup keeps the accounts its saved approval binds, which may be
        // none. Only a raised setup fee limit changes that approval.
        let ordered = self
            .record(operation)
            .is_some_and(|record| record.has_swap_use_order(retry.swap_use));
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
        // The setup's account reads go over the swap's route.
        let route = self.swap_route(&http, operation);
        // Only this account's stages start over. The other account's stay as they ended.
        let progress = self.start_setup_progress(operation, side, cx);
        let replaces_own = retry.replaces && side == SetupSide::Origin;
        let (replacements, followed) = Self::follow_replacements(cx);
        self.start_job(
            operation,
            SwapJobKind::Setup,
            async move {
                // The view is told each replacement at once, and again with the result.
                let mut replaced = Vec::new();
                let mut report = |from, swap| {
                    let account = ReplacedSwapAccount { from, swap };
                    replaced.push(account);
                    let _ = replacements.send(account);
                };
                let outcome = Box::pin(async {
                    let use_side = swap_use_side(side);
                    let mut swap = (operation, destination.as_ref().map(|(_, account)| *account));
                    if retry.replaces {
                        let replacement = owner.replace_swap_account(
                            destination.as_ref().map(|(owner, _)| owner.as_ref()),
                            operation,
                            retry.swap_use,
                            use_side,
                        )?;
                        report(operation, replacement);
                        swap.0 = replacement.operation;
                        if matches!(use_side, SwapUseSide::Destination) {
                            swap.1 = Some(replacement.replacement);
                        }
                    }
                    let orderbook = route.orderbook(&owner).await?;
                    let client = orderbook.http();
                    let mut accounts = SwapAccountReplacements::new(
                        owner.as_ref(),
                        destination
                            .as_ref()
                            .zip(swap.1)
                            .map(|((owner, _), account)| (owner.as_ref(), account)),
                        swap.0,
                        retry.swap_use,
                        &mut report,
                    );
                    let prepared = Box::pin(accounts.resume_setup(
                        use_side,
                        &retry.candidate,
                        &authorization,
                        client,
                    ))
                    .await?;
                    // The swap's approval binds the accounts as they are derived by now, which a
                    // preparation interrupted before their derivation left unbound, and a
                    // replacement left for its new account: the swap's own, and the destination
                    // account as its delivery's receiver. A reviewed fee above the approved limit
                    // is saved with them, before the setup is sent. Beside an order, the approval
                    // keeps its bindings and only the fee limit is saved.
                    match retry.approval {
                        Some(approval) => {
                            accounts.bind_approval(approval)?;
                        }
                        None if ordered => {}
                        None => {
                            owner.bind_swap_use_accounts(
                                accounts.operation(),
                                accounts.swap_use(),
                                None,
                            )?;
                        }
                    }
                    let request = SwapSetupRequest {
                        maximum_private_fee: retry.maximum_private_fee,
                        session,
                        authorization: &authorization,
                        waku: retry.waku,
                        verify_proof: true,
                        progress_tx: Some(progress),
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                        client: client.clone(),
                    };
                    let mut prepared = SwapPairSide::Setup(prepared);
                    let sent = match use_side {
                        SwapUseSide::Source => {
                            Box::pin(accounts.submit_setups((&mut prepared, Some(request)), None))
                                .await
                                .0
                        }
                        // The swap's own account takes no part in its destination's setup.
                        SwapUseSide::Destination => {
                            let mut own = existing_pair_side(&owner, accounts.operation())?;
                            Box::pin(accounts.submit_setups(
                                (&mut own, None),
                                Some((&mut prepared, Some(request))),
                            ))
                            .await
                            .1
                        }
                    };
                    let outcome =
                        sent.ok_or_else(|| eyre::eyre!("This swap has no setup to send."))??;
                    Ok::<_, eyre::Report>(outcome.result)
                })
                .await;
                Ok((replaced, outcome))
            },
            move |this, (replaced, result), window, cx| {
                // The swap is the one its last replacement left. The view follows here what
                // it didn't while the job ran, also when the job failed after a replacement.
                let moved = !replaced.is_empty();
                let operation = replaced
                    .last()
                    .map_or(operation, |account| account.swap.operation);
                for account in replaced.into_iter().skip(followed.finish()) {
                    this.follow_replaced_account(account, cx);
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        this.fail_setup_job(operation, &error, cx);
                        // A replacement that was refused leaves the swap at its retired
                        // account, whose stage doesn't say why the retry ended.
                        if replaces_own && !moved {
                            let message = this.job_error(&error, cx);
                            this.fail(operation, message);
                        }
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
