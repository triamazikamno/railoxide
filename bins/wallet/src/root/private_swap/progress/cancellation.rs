use super::*;

impl PrivateSwapsView {
    /// What this session's cancellation of `operation`'s preparation left, while the swap's
    /// detail still reports it. Another swap being prepared on the account takes the detail
    /// back.
    pub(super) fn cancelled_preparation(
        &self,
        operation: ExecutorOperationId,
    ) -> Option<&CancelledPreparation> {
        self.cancelled
            .as_ref()
            .filter(|cancelled| cancelled.operation == operation)
            .filter(|_| {
                self.record(operation)
                    .is_none_or(|record| self.pending_order(record).is_none())
            })
    }

    /// Rebuild cancellation details from encrypted use history after closing the dialog or
    /// restarting. A later swap on the same account must not replace this preparation's terms.
    pub(in crate::root::private_swap) fn cancelled_use(
        &self,
        swap: SwapIdentity,
        cx: &App,
    ) -> Option<CancelledPreparation> {
        let record = self.record(swap.operation)?;
        let claimed = crate::root::private_swap::model::cancelled_swap_uses(record)
            .find(|claimed| claimed.id() == swap.swap_use)?;
        let approval = claimed.approval()?;
        let account = |role,
                       chain_id,
                       record: Option<&ExecutorRecord>,
                       fresh,
                       address: Option<alloy::primitives::Address>| {
            let unresolved = record.is_some_and(|record| {
                if fresh {
                    // A later use took this source only once the work this preparation
                    // signed had resolved. What the account signs since is that use's.
                    let later_use = matches!(role, CancelledAccountRole::Source)
                        && record
                            .swap_uses()
                            .iter()
                            .skip_while(|later| later.id() != swap.swap_use)
                            .nth(1)
                            .is_some();
                    return !later_use && record.has_unresolved_issued_work();
                }
                let Some(SwapUseRole::Destination { shields, .. }) =
                    record.swap_use(swap.swap_use).map(SwapUseRecord::role)
                else {
                    return false;
                };
                shields
                    .iter()
                    .any(|hash| record.payload_state(*hash) != Some(ExecutorPayloadState::Resolved))
            });
            CancelledAccount {
                role,
                network: network_name(chain_id),
                account: address.map(|address| SwapStepAccount {
                    index: record.map(ExecutorRecord::index),
                    address,
                }),
                fresh,
                release: if unresolved {
                    SwapUseRelease::IssuedWorkRemains
                } else {
                    SwapUseRelease::Released
                },
                unknown: record.is_none(),
            }
        };
        let mut accounts = vec![account(
            CancelledAccountRole::Source,
            self.origin_chain_id,
            Some(record),
            claimed.is_fresh(),
            record.address(),
        )];
        if let Some((delivery, _)) = swap_use_destination(record, swap.swap_use) {
            // The linked record exists before derivation resolves the approval's receiver.
            let destination = crate::root::private_swap::destination_account_metadata(
                self.destinations.get(&swap).and_then(Option::as_ref),
                Some(swap.swap_use),
                claimed.approval(),
            );
            let fresh = destination.fresh_from_claim_or_approval();
            accounts.push(account(
                CancelledAccountRole::Destination,
                delivery.destination_chain,
                destination.record,
                fresh,
                destination
                    .record
                    .and_then(ExecutorRecord::address)
                    .or_else(|| (!delivery.receiver.is_zero()).then_some(delivery.receiver)),
            ));
        }
        let title = approval.tokens.map_or_else(
            || "Swap preparation".to_owned(),
            |tokens| {
                let bought = match approval.delivery {
                    SwapDelivery::Bridge(delivery) => self.network_token_symbol(
                        delivery.destination_chain,
                        self.delivered_token(delivery, cx),
                        cx,
                    ),
                    SwapDelivery::NonEvmBridge(ref delivery) => {
                        format!("{} on {}", delivery.symbol, delivery.network.display_name())
                    }
                    _ => self.token_symbol(tokens.buy, cx),
                };
                format!(
                    "{} → {bought}",
                    self.token_amount(tokens.sell, approval.bounds.spend_amount(), cx)
                )
            },
        );
        Some(CancelledPreparation {
            operation: swap.operation,
            title,
            accounts,
        })
    }

    pub(in crate::root::private_swap) fn render_cancelled_use(
        &self,
        swap: SwapIdentity,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        self.cancelled_use(swap, cx).map_or_else(
            || (app_muted_text("Swap preparation unavailable."), None),
            |cancelled| {
                let available = self.offers_cancelled_source(swap.operation);
                render_cancelled_preparation(&cancelled, available, cx)
            },
        )
    }

    /// Whether the Source picker offers `operation`'s account to a new swap now, by the same
    /// local admission and without a network request.
    pub(in crate::root::private_swap) fn offers_cancelled_source(
        &self,
        operation: ExecutorOperationId,
    ) -> bool {
        self.record(operation).is_some_and(|record| {
            swap_account_refusal(
                record,
                self.origin_chain_id,
                SwapAccountRole::Source,
                SwapAccountUse::New,
                SwapAdmissionEvidence::Recorded,
            )
            .is_none()
        })
    }

    pub(super) fn confirm_cancel_preparation(
        &self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(swap_use) = self.record(operation).and_then(Self::prepared_draft) else {
            return;
        };
        // A running setup may move the swap to an account that replaces a used one before the
        // user answers. The swap's use and the job's revision name them whatever it moves.
        let setup_job = self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup)
            .then_some(self.job_revision);
        let view = cx.entity().downgrade();
        window.open_wallet_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(app_strong_text("Cancel this swap's preparation?"))
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text("Keep preparation")
                        .ok_text("Cancel preparation")
                        .ok_variant(ButtonVariant::Danger),
                )
                .confirm()
                .child(app_text("No order will be placed. An existing stealth account is released for other swaps unless it already signed for this one. A setup already sent to a broadcaster may still confirm and charge its fee, and its new account stays in Stealth accounts for recovery.").whitespace_normal())
                .on_ok(move |_, _, cx| {
                    view.update(cx, |view, cx| view.cancel_prepared_use(swap_use, setup_job, cx))
                        .unwrap_or(false)
                })
        });
    }

    /// Cancel the preparation of swap use `id`, which has no order yet, through the owners:
    /// the use is stopped on both of its accounts in one write, and each account is released
    /// or kept as the native cancellation reports. The detail stays open and reports that
    /// outcome for each account. An existing account is never retired by this. The
    /// destination network's owner hears of the change when its session is loaded.
    ///
    /// `setup_job` is the revision of the setup job that ran for it when the user was asked.
    /// That job may have replaced the swap's used account since, which moves the use to
    /// another operation, and this view may not have followed it yet. So the job is stopped
    /// under whichever operation it is registered, the use is cancelled on the account that
    /// holds it by then, and the swap moves there before its cancellation is reported.
    fn cancel_prepared_use(
        &mut self,
        id: SwapUseId,
        setup_job: Option<u64>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.session_is_current(cx) {
            return false;
        }
        let Some(origin) = self.private_owner().cloned() else {
            return false;
        };
        let Some((record, swap_use)) = self.records.iter().find_map(|record| {
            let swap_use = prepared_swap_use(record).filter(|swap_use| swap_use.id() == id)?;
            Some((record, swap_use))
        }) else {
            return false;
        };
        // The records as this view read them last, and where it holds the swap: with its
        // setup job, which a followed replacement moves along with everything else.
        let known = record.operation();
        let operation = self
            .job
            .as_ref()
            .filter(|job| job.kind == SwapJobKind::Setup && setup_job == Some(self.job_revision))
            .map_or(known, |job| job.operation);
        let title = self.progress_title(known, cx);
        // What the use holds of each account, read before the cancellation changes it.
        let source = (
            CancelledAccountRole::Source,
            network_name(self.origin_chain_id),
            record.address().map(|address| SwapStepAccount {
                index: Some(record.index()),
                address,
            }),
            swap_use.is_fresh(),
        );
        let destination = swap_use_destination(record, id).map(|(delivery, _)| {
            let account = crate::root::private_swap::destination_account_metadata(
                self.swap_destination_account(
                    SwapIdentity {
                        operation: known,
                        swap_use: id,
                    },
                    delivery,
                ),
                Some(id),
                swap_use.approval(),
            );
            let fresh = account.fresh_from_claim_or_approval();
            (
                delivery.destination_chain,
                (
                    CancelledAccountRole::Destination,
                    network_name(delivery.destination_chain),
                    (!delivery.receiver.is_zero()).then(|| SwapStepAccount {
                        index: account.record.map(ExecutorRecord::index),
                        address: delivery.receiver,
                    }),
                    fresh,
                ),
            )
        });
        let destination_owner = destination
            .as_ref()
            .and_then(|(chain_id, _)| self.destination_owner(*chain_id, cx))
            .map(|(_, owner)| owner);
        // A setup still being handed off in this session stops with the preparation.
        self.stop_setup_job(operation);
        let (cancelled, cancellation) =
            match origin.cancel_swap_use_by_id(destination_owner.as_deref(), id) {
                Ok(cancelled) => cancelled,
                Err(error) => {
                    self.fail(operation, format!("{error:#}"));
                    cx.notify();
                    return false;
                }
            };
        // The use was cancelled on an account that replaced the one this view holds the swap
        // under. The swap moves there as it does when its setup job reports the replacement.
        if cancelled != operation {
            self.follow_replaced_account(
                ReplacedSwapAccount {
                    from: operation,
                    swap: SwapAccountReplacement {
                        operation: cancelled,
                        swap_use: id,
                        replacement: cancelled,
                    },
                },
                cx,
            );
        }
        let operation = cancelled;
        // The details name the account the use was cancelled on.
        let source = if operation == known {
            source
        } else {
            let (role, network, _, fresh) = source;
            let account = self.record(operation).and_then(|record| {
                Some(SwapStepAccount {
                    index: Some(record.index()),
                    address: record.address()?,
                })
            });
            (role, network, account, fresh)
        };
        let account = |(role, network, account, fresh), release| CancelledAccount {
            role,
            network,
            account,
            fresh,
            release,
            unknown: false,
        };
        let accounts = std::iter::once(account(source, cancellation.source))
            .chain(
                destination
                    .zip(cancellation.destination)
                    .map(|((_, destination), release)| account(destination, release)),
            )
            .collect();
        let tracking = self.tracking.entry(operation).or_default();
        tracking.pending_order = None;
        tracking.error = None;
        self.error = None;
        self.form = None;
        self.reload_records();
        self.reload_destinations(cx);
        self.cancelled = Some(CancelledPreparation {
            operation,
            title,
            accounts,
        });
        cx.notify();
        true
    }

    pub(super) fn request_cancel_quote(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some((sell, _)) = self.record(operation).and_then(swap_tokens) else {
            return;
        };
        let (_, token, candidates) =
            self.setup_fee_route(self.origin_chain_id, Some(sell), None, false, false, cx);
        let candidate = token.and_then(|_| {
            let root = self.root.upgrade()?;
            let root = root.read(cx);
            wallet_ops::select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &PublicBroadcasterSelection::Random,
                root.public_broadcaster_fee_policy(false),
                &root.public_broadcaster_trust_filter(false),
            )
            .ok()
        });
        let Some(candidate) = candidate else {
            self.fail(
                operation,
                "No compatible broadcaster accepts a private fee token you hold. Wait for the order to expire at no cost, or try again later."
                    .into(),
            );
            cx.notify();
            return;
        };
        let (Some(owner), Some(session), Some(http)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
            self.http(cx),
        ) else {
            return;
        };
        self.start_job(
            operation,
            SwapJobKind::CancelQuote,
            async move {
                let client = http.operation_http_client().await?;
                Box::pin(
                    owner.estimate_swap_cancellation_fee(operation, &session, candidate, &client),
                )
                .await
            },
            move |this, estimate, window, cx| {
                this.cancel = Some(CancelQuote {
                    operation,
                    estimate,
                });
                this.open_cancel_confirmation(window, cx);
            },
            window,
            cx,
        );
    }

    fn open_cancel_confirmation(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(quote) = self.cancel.as_ref() else {
            return;
        };
        let fee = self.token_amount(
            quote.estimate.broadcaster().token,
            quote.estimate.fee_amount(),
            cx,
        );
        let wait = self
            .record(quote.operation)
            .and_then(swap_valid_to)
            .map_or_else(
                || "If you wait, the order expires at no cost.".to_owned(),
                |valid_to| {
                    format!(
                        "If you wait, the order expires at {} at no cost.",
                        local_time_label(valid_to)
                    )
                },
            );
        let body = SharedString::from(format!(
            "Cancelling now costs about {fee} through a broadcaster. {wait} Anyone can trigger the swap's unshield before expiry. If the order then doesn't fill, recovery costs an unshield fee and a shield fee."
        ));
        let view = cx.entity().downgrade();
        window.open_wallet_alert_dialog(cx, move |dialog, window, _cx| {
            let confirm_view = view.clone();
            let cancel_view = view.clone();
            let width =
                (window.viewport_size().width * 0.92).min(rems(27.5).to_pixels(window.rem_size()));
            dialog
                .width(width)
                .max_h(dialog_max_height(window))
                .title(app_strong_text("Cancel the swap order now?"))
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text("Keep waiting")
                        .cancel_variant(ButtonVariant::Secondary)
                        .ok_text("Cancel order")
                        .ok_variant(ButtonVariant::Danger),
                )
                .confirm()
                .child(app_text(body.clone()).whitespace_normal())
                .on_cancel(move |_, _, cx| {
                    let _ = cancel_view.update(cx, |view, cx| {
                        view.cancel = None;
                        cx.notify();
                    });
                    true
                })
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    let _ = confirm_view.update(cx, |view, cx| {
                        view.request_cancel_authorization(window, cx);
                    });
                    false
                })
        });
    }

    fn request_cancel_authorization(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(quote) = self.cancel.take() else {
            return;
        };
        let operation = quote.operation;
        let Some(waku) = self.broadcaster_network(cx) else {
            self.fail(
                operation,
                "Wait for the broadcaster network connection, then try again.".into(),
            );
            cx.notify();
            return;
        };
        let approval = CancelApproval {
            operation,
            candidate: quote.estimate.broadcaster().clone(),
            maximum_private_fee: quote.estimate.fee_amount(),
            waku,
        };
        let order = self
            .record(operation)
            .map_or_else(String::new, |record| self.labels(record, cx).pair);
        let fee = self.token_amount(approval.candidate.token, approval.maximum_private_fee, cx);
        let summary = SpendAuthorizationSummary::new(
            "Cancel swap order",
            "Pay a broadcaster privately to make this swap's open order unfillable.",
            vec![
                SpendAuthorizationSummaryRow::new("Order", order),
                SpendAuthorizationSummaryRow::new("Private fee", format!("Up to {fee}")),
                SpendAuthorizationSummaryRow::new(
                    "Broadcaster",
                    broadcaster_candidate_label(&approval.candidate),
                ),
            ],
        )
        .with_context(
            "The cancellation takes the pre-hook's nonce, so the swap's unshield can't run afterwards. Anyone can trigger the unshield first, in which case the swap continues and the cancellation doesn't apply. If the order then doesn't fill, recovery costs an unshield fee and a shield fee.",
        )
        .with_confirm_label("Authorize cancellation")
        .requiring_explicit_review();
        self.request_authorization(SwapAction::Cancel(Box::new(approval)), summary, window, cx);
    }

    pub(in crate::root::private_swap) fn submit_cancellation(
        &mut self,
        approval: CancelApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let operation = approval.operation;
        let (Some(owner), Some(session), Some(http)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
            self.http(cx),
        ) else {
            return;
        };
        self.start_job(
            operation,
            SwapJobKind::Cancel,
            async move {
                // One route for the cancellation's reads and its paid execution.
                let client = http.operation_http_client().await?;
                let prepared = Box::pin(owner.prepare_swap_cancellation(
                    operation,
                    approval.candidate,
                    approval.maximum_private_fee,
                    &authorization,
                    &client,
                ))
                .await?;
                let outcome = Box::pin(owner.submit_paid_recovery(ExecutorPaidRecoveryRequest {
                    recovery: Arc::new(prepared),
                    session,
                    authorization,
                    waku: approval.waku,
                    verify_proof: true,
                    progress_tx: None,
                    response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                    republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                    client,
                }))
                .await?;
                Ok(outcome.result)
            },
            move |this, result, window, cx| {
                let problem = broadcaster_result_problem(&result, "cancellation");
                let tracking = this.tracking.entry(operation).or_default();
                tracking.cancelling = problem.is_none();
                tracking.error = problem;
                if !window.has_active_dialog(cx) {
                    this.show_detail(operation, window, cx);
                }
            },
            window,
            cx,
        );
    }
}
