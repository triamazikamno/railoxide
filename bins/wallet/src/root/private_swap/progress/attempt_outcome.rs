use super::*;

impl PrivateSwapsView {
    /// Every attempt of a swap that has more than one, oldest first: what it contributed or
    /// how it ended, its own approved minimum and its expiry. A Bridge order's minimum is on
    /// its destination network. A swap with one attempt has no rows.
    pub(in crate::root::private_swap) fn attempt_rows(
        &self,
        record: &ExecutorRecord,
        attempts: &SwapAttempts<'_>,
        cx: &App,
    ) -> Vec<SwapAttemptRow> {
        if attempts.attempts.len() < 2 {
            return Vec::new();
        }
        attempts
            .attempts
            .iter()
            .enumerate()
            .map(|(index, order)| {
                let role = attempts.role(order).map_or_else(
                    || {
                        swap_order_status(
                            swap_order_stage(record, order, self.attribution(record)),
                            false,
                            &SwapLabels::default(),
                        )
                    },
                    str::to_owned,
                );
                let bounds = order.bounds();
                let minimum = match order.delivery() {
                    SwapDelivery::Bridge(bridge) => bounds.destination_minimum.map(|minimum| {
                        self.network_token_amount(
                            bridge.destination_chain,
                            self.delivered_token(bridge, cx),
                            if bridge.is_private() {
                                private_delivery_credit(minimum, bounds)
                            } else {
                                minimum
                            },
                            cx,
                        )
                    }),
                    SwapDelivery::NonEvmBridge(delivery) => bounds
                        .destination_minimum
                        .map(|minimum| non_evm_amount(&delivery, minimum)),
                    _ => record.swap().map(|swap| {
                        self.token_amount(
                            swap.order_terms(order).buy_token(),
                            bounds.private_minimum,
                            cx,
                        )
                    }),
                };
                SwapAttemptRow {
                    label: format!("Order {} · {role}", index + 1),
                    order: order.uid(),
                    detail: [
                        minimum.map(|minimum| format!("Minimum {minimum}")),
                        Some(format!(
                            "valid until {}",
                            local_date_time_label(u64::from(order.valid_to()))
                        )),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                }
            })
            .collect()
    }

    /// The Orders section of a swap with more than one attempt: each attempt's
    /// [`SwapAttemptRow`], laid out like [`hash_row`] with a control that copies its order ID
    /// in full.
    pub(super) fn render_attempts(
        &self,
        record: &ExecutorRecord,
        attempts: &SwapAttempts<'_>,
        cx: &App,
    ) -> Option<gpui::Div> {
        let rows = self.attempt_rows(record, attempts, cx);
        (!rows.is_empty()).then(|| {
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .debug_selector(|| "swap-attempts".into())
                .child(app_strong_text("Orders"))
                .children(rows.into_iter().map(|row| {
                    let order_id = row.order.0.to_string();
                    let copy_id = SharedString::from(format!("swap-attempt-{order_id}-copy"));
                    let selector = format!("swap-attempt-{order_id}");
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .debug_selector(move || selector)
                        .child(hash_row(row.label, order_id, copy_id, "Copy order ID"))
                        .child(app_muted_text(row.detail).text_xs().whitespace_normal())
                }))
        })
    }

    /// What a traded order sold and delivered, from its approved bounds and canonical
    /// observations: the amounts, the price, the result against the minimum and the fees, then
    /// the order's details, collapsed. Rows without recorded data are left out. A Bridge
    /// swap's outcome is on its destination network, which [`Self::render_bridge_facts`] shows.
    pub(super) fn render_outcome(
        &self,
        record: &ExecutorRecord,
        order: &SwapOrderRecord,
        stage: SwapStage,
        started: Option<(&'static str, u64)>,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if order.delivery().is_bridge() {
            return None;
        }
        let SwapStage::Order(
            state @ (SwapOrderState::Traded | SwapOrderState::Done | SwapOrderState::NotDelivered),
        ) = stage
        else {
            return None;
        };
        let swap = record.swap()?;
        let outcome = swap_outcome(order.bounds(), &order.observations(), &order.delivery())?;
        let terms = swap.order_terms(order);
        let (sell, buy) = (terms.sell_token(), terms.buy_token());
        let amount =
            |token, value| self.with_usd(self.token_amount(token, value, cx), token, value, cx);
        let (fees_open, details_open) = self.dialog.as_ref().map_or((false, false), |dialog| {
            (dialog.outcome_fees_open, dialog.outcome_details_open)
        });

        let received = outcome
            .received_privately
            .map(|(private, _)| private)
            .or_else(|| outcome.trade.map(|trade| trade.buy_amount));
        let destination = self.receiver_name(&order.delivery(), cx).map_or_else(
            || {
                match state {
                    SwapOrderState::Done => "in your private balance",
                    SwapOrderState::NotDelivered => "in the stealth account",
                    _ => "arriving",
                }
                .to_owned()
            },
            // The trade paid the receiver in the same settlement.
            |receiver| format!("delivered to {receiver}"),
        );
        let with_usd_prefix = |usd: Option<String>, note: &str| {
            usd.map_or_else(|| note.to_owned(), |usd| format!("{usd} {note}"))
        };
        let amounts = div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .gap_3()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
            .child(outcome_side(
                self.token_icon(sell, cx),
                self.token_amount(sell, outcome.spent, cx),
                with_usd_prefix(self.usd_value(sell, outcome.spent, cx), "spent privately"),
                false,
            ))
            .child(
                app_muted_text("→")
                    .flex_none()
                    .text_size(theme::BALANCE_TEXT_SIZE),
            )
            .child(outcome_side(
                self.token_icon(buy, cx),
                received.map_or_else(
                    || format!("at least {}", self.token_amount(buy, outcome.minimum, cx)),
                    |received| self.token_amount(buy, received, cx),
                ),
                with_usd_prefix(
                    received.and_then(|received| self.usd_value(buy, received, cx)),
                    destination.as_str(),
                ),
                true,
            ));

        let mut rows = Vec::new();
        if let SwapDelivery::External { receiver } = order.delivery() {
            rows.push(self.receiver_row(receiver, cx).into_any_element());
        }
        let limit = self.pair_rate_label(sell, buy, outcome.limit_sell, outcome.minimum, cx);
        let (price, limit) = match outcome.trade.and_then(|trade| {
            self.pair_rate_label(sell, buy, trade.sell_amount, trade.buy_amount, cx)
        }) {
            Some(executed) => (Some(("Price", executed)), limit),
            None => (limit.map(|limit| ("Limit price", limit)), None),
        };
        rows.extend(price.map(|(label, price)| outcome_row(label, price, None).into_any_element()));
        // What was delivered against the approved minimum, then the fee the orderbook charged
        // beside the settlement transaction's gas cost, each once recorded.
        rows.extend(outcome.received.map(|received| {
            fact_row(
                "Received",
                div()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_1()
                    .child(app_text(amount(buy, received)))
                    .children(outcome.above_minimum.map(|above| {
                        app_text(format!(
                            "{} above your minimum",
                            self.bare_amount(buy, above, cx)
                        ))
                        .text_color(cx.theme().success)
                    })),
            )
            .debug_selector(|| "swap-outcome-received".into())
            .into_any_element()
        }));
        rows.push(
            self.minimum_row(buy, outcome.private_minimum, order, cx)
                .into_any_element(),
        );
        rows.extend(outcome.gas.map(|gas| {
            // The executed fee is everything the order was charged, not its gas alone, so the
            // two figures aren't compared.
            fact_row(
                "CoW fee",
                div()
                    .id("swap-outcome-gas-value")
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_1()
                    .tooltip(|window, cx| {
                        Tooltip::new(
                            "What CoW charged this order, network and protocol fees together, beside what the settlement's gas cost",
                        )
                        .build(window, cx)
                    })
                    .child(app_text(format!(
                        "{} charged",
                        self.money(gas.fee_token, gas.fee, cx)
                    )))
                    .child(app_muted_text(format!(
                        "· settlement gas {}",
                        self.money(Address::ZERO, gas.settlement_cost, cx)
                    ))),
            )
            .debug_selector(|| "swap-outcome-gas".into())
            .into_any_element()
        }));
        let unshield_fee = Some(outcome.unshield_fee).filter(|fee| !fee.is_zero());
        let shield_fee = outcome
            .received_privately
            .and_then(|(_, fee)| fee)
            .filter(|fee| !fee.is_zero());
        let order_fee = outcome
            .trade
            .map(|trade| trade.fee_amount)
            .filter(|fee| !fee.is_zero());
        let railgun_fees = [
            unshield_fee.map(|fee| (sell, fee)),
            shield_fee.map(|fee| (buy, fee)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !railgun_fees.is_empty() || order_fee.is_some() {
            // Two tokens, so one USD total, or none unless every fee has a rate.
            let usd = railgun_fees
                .iter()
                .try_fold(U256::ZERO, |total, &(token, fee)| {
                    Some(total.saturating_add(self.usd_micro_value(token, fee, cx)?))
                });
            let summary = (!railgun_fees.is_empty()).then(|| {
                usd.map_or_else(
                    || {
                        railgun_fees
                            .iter()
                            .map(|&(token, fee)| self.token_amount(token, fee, cx))
                            .collect::<Vec<_>>()
                            .join(" + ")
                    },
                    |usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)),
                )
            });
            let fees = outcome_disclosure(
                "swap-outcome-fees",
                "Fees",
                summary.map(app_text),
                fees_open,
                cx.listener(|this, _, _, cx| {
                    if let Some(dialog) = this.dialog.as_mut() {
                        dialog.outcome_fees_open = !dialog.outcome_fees_open;
                        cx.notify();
                    }
                }),
            );
            let fees = if fees_open {
                let mut fee_rows = Vec::new();
                fee_rows.extend(
                    unshield_fee.map(|fee| outcome_row("Unshield", amount(sell, fee), None)),
                );
                fee_rows
                    .extend(order_fee.map(|fee| outcome_row("Order fee", amount(sell, fee), None)));
                fee_rows
                    .extend(shield_fee.map(|fee| outcome_row("Shield", amount(buy, fee), None)));
                fee_rows.push(
                    app_muted_text("CoW's network and protocol fees are included in the price.")
                        .text_xs()
                        .whitespace_normal(),
                );
                fees.content(outcome_nested(fee_rows))
            } else {
                fees
            };
            rows.push(fees.into_any_element());
        }

        let details = outcome_disclosure(
            "swap-outcome-details",
            "Order details",
            started
                .filter(|&(label, _)| !details_open && label == "Started")
                .map(|(_, at)| app_muted_text(local_date_time_label(at))),
            details_open,
            cx.listener(|this, _, _, cx| {
                if let Some(dialog) = this.dialog.as_mut() {
                    dialog.outcome_details_open = !dialog.outcome_details_open;
                    cx.notify();
                }
            }),
        )
        .pt_3()
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE));
        let details = if details_open {
            let mut detail_rows = Vec::new();
            detail_rows.extend(
                started.map(|(label, at)| fact_row(label, app_text(local_date_time_label(at)))),
            );
            if let Some(trade) = outcome.trade {
                detail_rows.push(outcome_row(
                    "Sold to CoW",
                    amount(sell, trade.sell_amount),
                    None,
                ));
                detail_rows.push(outcome_row(
                    "Received from CoW",
                    amount(buy, trade.buy_amount),
                    None,
                ));
            }
            detail_rows.extend(limit.map(|limit| outcome_row("Limit price", limit, None)));
            let order_id = order.uid().0.to_string();
            let copy_id = SharedString::from(format!("swap-order-{order_id}-copy"));
            detail_rows.push(hash_row("Order ID", order_id, copy_id, "Copy order ID"));
            detail_rows.push(fact_row("Settled by", settled_by_cow()));
            detail_rows.extend(outcome.settlement.map(|hash| {
                let hash = hash.to_string();
                let copy_id = SharedString::from(format!("swap-settlement-{hash}-copy"));
                hash_row(
                    "Settlement",
                    hash,
                    copy_id,
                    "Copy settlement transaction hash",
                )
            }));
            details.content(outcome_nested(detail_rows))
        } else {
            details
        };
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(amounts)
                .child(div().w_full().flex().flex_col().gap_2().children(rows))
                .child(details)
                .debug_selector(|| "swap-outcome".into()),
        )
    }

    /// The approved minimum of `buy` and, when recorded, the gas share it was set with.
    fn minimum_row(
        &self,
        buy: Address,
        minimum: U256,
        order: &SwapOrderRecord,
        cx: &App,
    ) -> gpui::Div {
        fact_row(
            "Minimum",
            amount_with_note(
                self.with_usd(self.token_amount(buy, minimum, cx), buy, minimum, cx),
                order
                    .bounds()
                    .gas_share_bps
                    .map(gas_share_name)
                    .unwrap_or_default(),
            ),
        )
        .debug_selector(|| "swap-outcome-minimum".into())
    }

    /// An order that expired unfilled, as a normal outcome: its minimum, the gas it allowed
    /// against the estimate, when it expired and the gas price when it was signed, then where
    /// the inputs are. Rows a record from before gas shares lacks are left out. A Bridge
    /// swap's facts explain its own expiry.
    pub(super) fn render_not_filled(
        &self,
        record: &ExecutorRecord,
        order: &SwapOrderRecord,
        stage: SwapStage,
        cx: &App,
    ) -> Option<gpui::Div> {
        if stage != SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired))
            || order.delivery().is_bridge()
        {
            return None;
        }
        let terms = record.swap()?.order_terms(order);
        let (sell, buy) = (terms.sell_token(), terms.buy_token());
        let bounds = order.bounds();
        let mut rows = vec![self.minimum_row(buy, bounds.private_minimum, order, cx)];
        if let (Some(allowance), Some(estimate)) = (bounds.gas_allowance, bounds.gas_estimate) {
            rows.push(
                fact_row(
                    "Gas you allowed",
                    app_text(format!(
                        "up to {} of ≈ {}",
                        self.money(buy, allowance, cx),
                        self.money(buy, estimate, cx)
                    )),
                )
                .debug_selector(|| "swap-not-filled-gas".into()),
            );
        }
        rows.push(fact_row(
            "Expired",
            app_text(local_date_time_label(u64::from(order.valid_to()))),
        ));
        rows.extend(bounds.gas_price_wei.map(|price| {
            fact_row(
                "Gas price at signing",
                app_text(format!("{} gwei", format_gwei(price))),
            )
            .debug_selector(|| "swap-not-filled-gas-price".into())
        }));
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().w_full().flex().flex_col().gap_2().children(rows))
                .child(
                    app_muted_text(format!(
                        "Nothing was unshielded. Your {} is still in your private balance. The setup fee isn't refunded.",
                        self.token_amount(sell, bounds.spend_amount(), cx)
                    ))
                    .whitespace_normal(),
                )
                .debug_selector(|| "swap-not-filled".into()),
        )
    }

    pub(super) fn render_progress_actions(
        &self,
        operation: ExecutorOperationId,
        stage: SwapStage,
        actions: crate::root::private_swap::model::SwapActions,
        job: Option<SwapJobKind>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let busy = self.busy();
        let record = self.record(operation);
        let hidden =
            record.is_some_and(|record| record.is_hidden() && self.pending_order(record).is_none());
        let setup_job = self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
        // A prepared swap is cancelled as a whole, including when its source is new.
        let prepared = record.and_then(Self::prepared_draft);
        let cancel_preparation = prepared.map(|_| {
            app_button("swap-progress-cancel-preparation", "Cancel preparation…")
                .debug_selector(|| "swap-progress-cancel-preparation".into())
                .small()
                .flex_none()
                .disabled(busy && !setup_job)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_cancel_preparation(operation, window, cx);
                }))
        });
        let resubmit = self
            .record(operation)
            .and_then(|record| record.swap())
            .and_then(|swap| swap.orders().last())
            .filter(|order| order.submission_status() == SwapSubmissionStatus::Pending)
            .filter(|_| {
                matches!(
                    stage,
                    SwapStage::SubmissionPending
                        | SwapStage::Order(SwapOrderState::PreHookOnly { expired: false })
                )
            })
            .map(|order| {
                let available = order.submission().is_some()
                    && u64::from(order.valid_to()) > now_unix().saturating_add(60);
                app_button("swap-progress-resubmit", "Submit again")
                    .primary()
                    .small()
                    .disabled(busy || !available)
                    .when(!available, |button| {
                        button.tooltip("Wait for final expiry, then retry with a new quote.")
                    })
                    .loading(job == Some(SwapJobKind::Order))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.resubmit_order(operation, window, cx);
                    }))
            });
        // A Bridge swap's check asks its provider again about a deposit that needs attention,
        // that Across refunds or whose delivery Across only reported, and reads the stealth
        // account for a refund or a deposit that wasn't sent.
        let across_refund = stage == SwapStage::Order(SwapOrderState::Refunding)
            && record.is_some_and(refunds_across_deposit);
        let across_reported = stage == SwapStage::Order(SwapOrderState::Done)
            && record.is_some_and(reported_by_across);
        let bridge_check = record.is_some_and(|record| {
            swap_delivery(record).is_bridge()
                && (across_reported
                    || matches!(
                        stage,
                        SwapStage::Order(
                            SwapOrderState::NeedsAttention
                                | SwapOrderState::Refunding
                                | SwapOrderState::NotDelivered
                        )
                    ))
        });
        // Held proceeds are looked for in the destination stealth account, on its network.
        let held = stage.is_held_on_destination();
        let check = record
            .and_then(swap_valid_to)
            .filter(|valid_to| {
                bridge_check
                    || held
                    || (stage.is_observed()
                        && (*valid_to < now_unix()
                            || stage == SwapStage::Order(SwapOrderState::Traded)))
            })
            .map(|_| {
                let checking = job == Some(SwapJobKind::Check);
                let label = if checking {
                    "Checking status"
                } else {
                    "Check status"
                };
                app_button("swap-progress-check", label)
                    .debug_selector(|| "swap-progress-check".into())
                    .outline()
                    .small()
                    .disabled(busy)
                    .when(checking, |button| button.icon(IconName::LoaderCircle))
                    .loading(checking)
                    .tooltip(match stage {
                        _ if held => "Checks the destination stealth account's balance with that network's RPC provider",
                        _ if !bridge_check => "Checks this stealth account's state with the RPC provider for retry or recovery",
                        SwapStage::Order(SwapOrderState::NeedsAttention) => {
                            "Asks the provider about the deposit, then checks this stealth account for a refund"
                        }
                        _ if across_refund => {
                            "Asks Across about the deposit and confirms its refund, then checks this stealth account's balance"
                        }
                        _ if across_reported => {
                            "Asks Across about the deposit again and checks its delivery on the destination network"
                        }
                        _ => "Checks this stealth account's balance with the RPC provider",
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if held {
                            this.check_destination_status(operation, window, cx);
                        } else if bridge_check {
                            this.check_bridge_status(operation, window, cx);
                        } else {
                            this.check_order_status(operation, window, cx);
                        }
                    }))
            });
        // A refund is recovered once an explicit check finds it in the stealth account. Kept
        // surplus can be there before an Across refund, so its refund must be verified first.
        let recover_blocked = if stage == SwapStage::Order(SwapOrderState::Refunding) {
            match self
                .tracking
                .get(&operation)
                .and_then(|tracking| tracking.stealth_balance)
            {
                _ if across_refund
                    && record
                        .and_then(latest_bridge_evidence)
                        .is_none_or(|order| order.observations().bridge_refund.is_none()) =>
                {
                    Some("Check status to confirm Across's refund to the stealth account.")
                }
                Some((balance, _)) if !balance.is_zero() => None,
                Some(_) => {
                    Some("The refund isn't in the stealth account yet. Check status again later.")
                }
                None => Some("Check status to find the refund in the stealth account."),
            }
        } else {
            None
        };
        let recover_is_next = stage.needs_recovery();
        let cancel = actions.cancel.map(|availability| {
            app_button("swap-progress-cancel-order", "Cancel order…")
                .debug_selector(|| "swap-progress-cancel-order".into())
                .danger()
                .small()
                .flex_none()
                .loading(job == Some(SwapJobKind::CancelQuote))
                .disabled(busy || availability.is_err())
                .when_some(availability.err(), gpui_component::button::Button::tooltip)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.request_cancel_quote(operation, window, cx);
                }))
        });
        // An approved order whose accounts are all ready is placed from here, as the wallet
        // does by itself after a setup approved in this session.
        let place = stage == SwapStage::Approved
            || record.is_some_and(|record| self.approved_use(record).is_some());
        let resume = actions.resume.map(|availability| {
            app_button(
                "swap-progress-continue",
                match stage {
                    _ if place => "Place order…",
                    SwapStage::SetupRetired => "Retry with new account…",
                    SwapStage::Ready => "Review…",
                    SwapStage::SetupPending => "Retry setup…",
                    _ => "Continue…",
                },
            )
            // Sending a setup again while it still waits isn't the next step.
            .when(
                place || stage != SwapStage::SetupPending,
                ButtonVariants::primary,
            )
            .small()
            .flex_none()
            .debug_selector(|| "swap-progress-continue".into())
            .loading(place && job.is_some())
            .disabled((busy && !setup_job) || availability.is_err())
            .when_some(availability.err(), gpui_component::button::Button::tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                // A setup shown included places its order; only a setup sent again waits.
                if this
                    .record(operation)
                    .is_none_or(|record| !place && this.setup_retry_problem(record).is_some())
                {
                    return;
                }
                if place {
                    this.place_approved_order(operation, window, cx);
                } else {
                    this.stop_setup_job(operation);
                    this.resume_swap(operation, window, cx);
                }
            }))
        });
        let expired =
            stage == SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired));
        let retry = actions.retry.map(|availability| {
            app_button(
                "swap-progress-retry",
                if expired { "Swap again…" } else { "Retry…" },
            )
            .debug_selector(|| "swap-progress-retry".into())
            .when(availability.is_ok(), ButtonVariants::primary)
            .small()
            .flex_none()
            .disabled(busy || availability.is_err())
            .when_some(availability.err(), gpui_component::button::Button::tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_existing_form(operation, window, cx);
            }))
        });
        let recover = actions.recover.then(|| {
            app_button("swap-progress-recover", "Recover…")
                .debug_selector(|| "swap-progress-recover".into())
                .when(recover_is_next, ButtonVariants::primary)
                .small()
                .flex_none()
                .disabled(busy || recover_blocked.is_some())
                .tooltip(recover_blocked.unwrap_or("Opens this swap's stealth account recovery"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.recover(operation, window, cx);
                }))
        });
        // Recovery of held proceeds is the destination stealth account's, on its own network,
        // once that network's records tell the account.
        let recover_on_destination = record
            .filter(|_| actions.recover_on_destination)
            .and_then(|record| Some((record, swap_private_delivery(record)?)))
            .map(|(record, delivery)| {
                let network = network_name(delivery.destination_chain);
                let loaded = self.destination_account(record, delivery).is_some();
                app_button(
                    "swap-progress-recover-destination",
                    format!("Recover on {network}…"),
                )
                .debug_selector(|| "swap-progress-recover-destination".into())
                .small()
                .flex_none()
                .disabled(busy || !loaded)
                .tooltip(if loaded {
                    format!("Switches to {network} and opens the stealth account's recovery there")
                } else {
                    format!("Available once {network} has loaded")
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.recover_on_destination(operation, window, cx);
                }))
            });
        // A removed swap stays in My orders, so its detail opens without this action.
        let remove = (actions.dismiss && !hidden && prepared.is_none()).then(|| {
            app_button("swap-progress-remove", "Remove from Private tab")
                .debug_selector(|| "swap-progress-remove".into())
                .small()
                .flex_none()
                .disabled(busy)
                .tooltip(
                    "Hides this card. Tracking and reserved funds are unchanged. The swap stays in My orders, and its account stays in Stealth accounts.",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_from_private_tab(operation, cx);
                }))
        });
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .children(cancel)
            .children(cancel_preparation)
            .children(remove)
            .child(div().flex_1())
            .child(
                app_button("swap-progress-close", "Close")
                    .small()
                    .flex_none()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_swap_dialog(window, cx);
                    })),
            )
            .children(resubmit)
            .children(resume)
            .children(retry)
            .children(check)
            .children(recover)
            .children(recover_on_destination)
    }

    /// Stop local delivery and automatic continuation. Already issued payloads remain
    /// recorded and observed because a broadcaster can still submit them.
    pub(super) fn stop_setup_job(&mut self, operation: ExecutorOperationId) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup)
        {
            if let Some(job) = self.job.take() {
                job.abort.abort();
            }
            self.job_revision = self.job_revision.wrapping_add(1);
        }
        if let Some(tracking) = self.tracking.get_mut(&operation) {
            tracking.auto_place = false;
            tracking.clear_setup_progress();
        }
    }

    /// A claimed source use without an order, whether its source account is new or reused.
    pub(super) fn prepared_draft(record: &ExecutorRecord) -> Option<SwapUseId> {
        prepared_swap_use(record)
            .filter(|swap_use| swap_use.approval().is_some())
            .map(SwapUseRecord::id)
    }
}
