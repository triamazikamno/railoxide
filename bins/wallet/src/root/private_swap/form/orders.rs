use super::*;

impl PrivateSwapsView {
    /// Place the order approved with the setup once every setup the swap needs is confirmed:
    /// see [`Self::approved_use`]. The approved amount is planned and quoted again first;
    /// unchanged terms need only a confirm-only step, which a remembered spend authorization
    /// satisfies without a prompt.
    pub(in crate::root::private_swap) fn place_approved_order(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let (Some(owner), Some(session)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
        ) else {
            return;
        };
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some((swap_use, approval)) = self.approved_use(record) else {
            return;
        };
        // A reused account's earlier swaps have their own terms, so its pair is the approved one.
        let Some((sell, buy)) = approval
            .tokens
            .map(|tokens| (tokens.sell, tokens.buy))
            .or_else(|| swap_tokens(record))
        else {
            return;
        };
        let reuse = record
            .swap_use(swap_use)
            .is_some_and(|claimed| !claimed.is_fresh());
        let approval = approval.clone();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (anchor_cache, tokens, destination_chain) = {
            let root = root.read(cx);
            let destination_chain = match approval.delivery {
                SwapDelivery::Bridge(delivery) => root
                    .effective_chain_configs
                    .get(delivery.destination_chain)
                    .cloned(),
                _ => None,
            };
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
                destination_chain,
            )
        };
        // The approved destination is looked up again in its provider's list. A non-EVM
        // network has no chain configuration to look up.
        let destination = match &approval.delivery {
            SwapDelivery::NonEvmBridge(_) => Some(QuoteBridgeDestination::NonEvm(None)),
            _ => destination_chain.map(|chain| QuoteBridgeDestination::Chain {
                destination: None,
                chain,
            }),
        };
        let bridge =
            destination
                .zip(self.origin_bridge_profile(cx))
                .map(|(destination, origin)| QuoteBridgeRequest {
                    destination,
                    origin,
                });
        let valid_for = self.default_valid_for(cx);
        let inclusion = self
            .record(operation)
            .and_then(|record| self.setup_inclusion(record));
        let http = root.read(cx).http.clone();
        let tracking = self.tracking.entry(operation).or_default();
        let request = QuoteRequest {
            http,
            executor: QuoteExecutor::Order { operation, reuse },
            inclusion,
            sell,
            buy,
            amount: approval.bounds.spend_amount(),
            delivery: approval.delivery.clone(),
            slippage_bps: approval.bounds.slippage_bps,
            // A legacy approval without a share needs a full review whatever the requote uses.
            gas_share_bps: approval
                .bounds
                .gas_share_bps
                .unwrap_or(GAS_SHARE_BALANCED_BPS),
            valid_for: approval
                .bounds
                .valid_for_secs
                .map_or(valid_for, |secs| Duration::from_secs(secs.into())),
            byte_budget: tracking.byte_budget,
            orderbook: tracking.orderbook.clone(),
            // The bridge clients this session keeps on the swap's orderbook route.
            bridge_clients: tracking.bridge_clients.clone(),
            bridge,
            anchor_cache: Some(anchor_cache),
            tokens,
        };
        self.start_job(
            operation,
            SwapJobKind::Requote,
            async move { Ok::<_, eyre::Report>(quote_swap(owner, session, request).await) },
            move |this, result, window, cx| {
                this.apply_approved_quote(operation, swap_use, &approval, result, window, cx);
            },
            window,
            cx,
        );
    }

    pub(super) fn apply_approved_quote(
        &mut self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        approval: &SwapApproval,
        result: QuoteResult,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let tracking = self.tracking.entry(operation).or_default();
        if let Some(orderbook) = &result.orderbook {
            tracking.set_route(Some(orderbook.clone()));
        }
        // Whatever the answer, the next attempt waits for the user.
        tracking.auto_place = false;
        let outcome = match result.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // The next attempt gets a new isolated route instead of the failed circuit.
                self.tracking.entry(operation).or_default().set_route(None);
                // The quote's values stay in the form's Details, out of the swap's failure text.
                let message = self.quote_error_message(&error, cx);
                self.fail(operation, message);
                return;
            }
        };
        // Keep the detail visible during the request. Only another dialog defers
        // authorization or review; failures remain available without an automatic retry.
        if window.has_active_dialog(cx) && !self.detail_is_active(operation, window, cx) {
            self.tracking.entry(operation).or_default().auto_place = true;
            return;
        }
        // An approval made from a preview without the shortfall is a change now, not after
        // another signing round.
        let outcome = match outcome {
            QuoteOutcome::Review(review) => QuoteOutcome::Review(Box::new(
                Self::with_tracked_delivery(&self.tracking, Some(operation), *review),
            )),
            outcome => outcome,
        };
        let change = match outcome {
            QuoteOutcome::Review(review) => match review.approved_order_minimum(approval) {
                // The approved minimum, or a Bridge deposit raised to keep the approved
                // destination minimum.
                Ok(private_minimum) => {
                    let Some(orderbook) = result.orderbook else {
                        return;
                    };
                    let approval = OrderApproval {
                        operation,
                        review: Arc::from(review),
                        private_minimum,
                        price_acknowledged: approval.price_acknowledged,
                        orderbook,
                        bridge: result.bridge,
                        destination_minimum: approval.bounds.destination_minimum,
                        // A first order has no earlier one, and signing checks only a
                        // speed-up for one.
                        continuing_minimum: None,
                        full_review: false,
                        // The use whose saved approval this step confirms.
                        swap_use: Some(swap_use),
                        pair_destination: None,
                        speed_up: None,
                    };
                    let summary = match self.place_summary(&approval, cx) {
                        Ok(summary) => summary,
                        Err(error) => {
                            self.fail(operation, format!("{error:#}"));
                            return;
                        }
                    };
                    self.request_authorization(
                        SwapAction::Order(Box::new(approval)),
                        summary,
                        window,
                        cx,
                    );
                    return;
                }
                Err(change) => Some(change),
            },
            QuoteOutcome::PriceBlocked(PriceBlock::Deviates) => {
                Some(SwapReviewChange::QuoteDeviates)
            }
            // The form shows the largest amount that fits.
            QuoteOutcome::TooLarge(_) => None,
        };
        // Nothing was signed. The form quotes the current terms and, once they are ready,
        // opens the review with the change named.
        self.reapproval = change.map(|change| (operation, change));
        self.open_existing_form(operation, window, cx);
    }

    /// The confirm-only step for an order approved with its setup, whose terms still hold. Its
    /// Receive card names the approved receiver of a Public address or Bridge swap once more,
    /// and a Bridge swap's destination terms the approval binds. The gas shown is what the
    /// signed minimum leaves room for.
    pub(super) fn place_summary(
        &self,
        approval: &OrderApproval,
        cx: &App,
    ) -> eyre::Result<SpendAuthorizationSummary> {
        let review = &approval.review;
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = review.valid_for().as_secs() / 60;
        let tolerance = format_bps_percent(u64::from(review.slippage_bps()));
        let mut details = vec![(
            "Gas you pay",
            format!(
                "Up to {} of ≈ {}",
                self.gas_money(buy, review.gas_allowance_for(approval.private_minimum)?, cx),
                self.gas_money(buy, review.gas_estimate(), cx)
            ),
        )];
        if let Some(fee) = review.cow_fee() {
            details.push(("CoW fee", self.token_amount(buy, fee, cx)));
        }
        details.push(("Price tolerance", tolerance.clone()));
        details.push(("Order valid for", format!("{valid_for} minutes from now")));
        let bridge = match plan.delivery() {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        let non_evm = match plan.delivery() {
            SwapDelivery::NonEvmBridge(delivery) => Some(delivery),
            _ => None,
        };
        let receive = match (
            bridge,
            approval.review.bridge(),
            approval.destination_minimum,
            &non_evm,
        ) {
            (_, Some(quote), Some(minimum), Some(delivery)) => {
                let expected = Some(quote.expected_output).filter(|expected| {
                    approval.private_minimum == review.suggested_private_minimum()
                        && *expected >= minimum
                });
                self.non_evm_receive_card(
                    delivery,
                    expected,
                    minimum,
                    one_click_usd_estimate(quote, minimum),
                    cx,
                )
            }
            // The private balance gets the approved minimum less the shield fee there.
            (Some(delivery), Some(quote), Some(minimum), _) if delivery.is_private() => {
                let approved = SwapBridgeQuote {
                    destination_minimum: minimum,
                    ..*quote
                };
                self.bridge_receive_card(delivery, None, approved.received_minimum(), cx)
            }
            (Some(delivery), Some(quote), Some(minimum), _) => {
                // The estimate was quoted for the review's own deposit.
                let expected = Some(quote.expected_output).filter(|expected| {
                    approval.private_minimum == review.suggested_private_minimum()
                        && *expected >= minimum
                });
                self.bridge_receive_card(delivery, expected, minimum, cx)
            }
            _ => self.receive_card(&approval.review, approval.private_minimum, cx),
        };
        let private = bridge.filter(BridgeDelivery::is_private);
        // The accounts as the approval binds them, and how many of them the swap set up.
        let accounts = self.review_accounts(Some(approval.operation), review);
        let setups = if accounts.is_empty() {
            usize::from(!plan.swap_executor().is_reused())
        } else {
            accounts.iter().filter(|account| !account.reused).count()
        };
        let rows = bridge
            .map(|delivery| self.bridge_row(delivery, buy, None, false, cx))
            .into_iter()
            .chain(
                non_evm
                    .as_ref()
                    .map(|_| self.non_evm_bridge_row(buy, None, cx)),
            )
            .chain(non_evm.as_ref().and_then(non_evm_contract_row))
            .chain(private.map(|delivery| self.shield_failure_row(delivery, buy, cx)))
            .chain(accounts.iter().map(ReviewAccount::row))
            .collect();
        let checked = bridge.map_or_else(
            || {
                if non_evm.is_some() {
                    return "The order keeps the minimum you approved. Costs and the NEAR Intents quote were checked again.".to_owned();
                }
                "The order keeps the minimum you approved. Costs were checked again and are shown below."
                    .to_owned()
            },
            |delivery| {
                if delivery.is_private() {
                    format!(
                        "The order keeps the minimum you approved. Costs, the {} quote and the shield fee on {} were checked again.",
                        provider_name(delivery.provider),
                        network_name(delivery.destination_chain)
                    )
                } else {
                    format!(
                        "The order keeps the minimum you approved. Costs and the {} quote were checked again.",
                        provider_name(delivery.provider)
                    )
                }
            },
        );
        let summary = SpendAuthorizationSummary::new("Place swap order", checked, rows);
        // The stepper names the setups that came before this step. A swap that reuses every
        // account had none, so its order is the only step.
        let summary = match setups {
            0 => summary,
            setups => summary.with_steps(2, swap_steps(setups > 1), swap_steps_hint(setups > 1)),
        };
        Ok(summary
            .with_cards(
                self.sell_card(
                    sell,
                    plan.amount(),
                    self.crosses_networks(bridge, non_evm.is_some()),
                    cx,
                ),
                receive,
            )
            .with_compact_rows()
            .with_details(
                "Order terms",
                format!(
                    "{} · {tolerance} price · {valid_for} min from now",
                    gas_share_name(review.gas_share_bps())
                ),
                details,
                None,
            )
            .with_confirm_label("Place order")
            .with_warnings(if approval.review.price_verified() {
                Vec::new()
            } else {
                vec![Arc::from(unverified_price_warning(review))]
            }))
    }

    /// Sign and submit the approved order. A private Bridge order also pre-signs its shield on
    /// the destination network, through that network's session and owner with
    /// `destination_authorization`.
    pub(in crate::root::private_swap) fn submit_order(
        &mut self,
        approval: OrderApproval,
        authorization: DesktopPrivateSpendAuthorization,
        destination_authorization: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        // A speed-up is signed only from its own explicit review, and only for the open order
        // that review named, which the plan binds through signing.
        let speed_up = approval.review.plan().speed_up();
        if approval.speeds_up()
            && (!approval.full_review
                || speed_up.map(|speed_up| speed_up.order) != approval.speed_up)
        {
            self.fail(
                approval.operation,
                "This speed-up wasn't reviewed for the swap's open order. Nothing was signed."
                    .into(),
            );
            cx.notify();
            return;
        }
        let (Some(owner), Some(session)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
        ) else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let destination = match approval.private_delivery() {
            Some(delivery) => {
                let context = self
                    .ready_destination(delivery.destination_chain, cx)
                    .and_then(|(session, owner)| {
                        Ok(SwapDestinationContext {
                            owner,
                            session,
                            authorization: destination_authorization
                                .ok_or(DESTINATION_AUTHORIZATION_MISSING)?,
                        })
                    });
                match context {
                    Ok(context) => Some(context),
                    Err(error) => {
                        self.fail(approval.operation, error);
                        cx.notify();
                        return;
                    }
                }
            }
            None => None,
        };
        let (anchors, tokens) = {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        };
        let operation = approval.operation;
        let plan = approval.review.plan();
        let swap_use = approval.swap_use.unwrap_or_else(|| {
            self.record(operation)
                .and_then(ExecutorRecord::active_swap_use)
                .unwrap_or_else(|| SwapUseId::first(operation))
        });
        let record = self.record(operation);
        let first_order = record.is_none_or(|record| {
            crate::root::private_swap::model::swap_use_last_order(record, swap_use).is_none()
        });
        // A reused account's order to an existing destination account claims both accounts
        // for its swap use before the order is signed. That saves the reviewed terms with the
        // accounts as they are chosen now, each without a setup, also when the review is
        // approved again before the swap's first order.
        let pair_destination = approval.pair_destination.filter(|_| first_order);
        // The first order of a swap use must match the approval saved for that use: with its
        // setup, or with a reused account's claim. After a full review the user authorized,
        // the reviewed terms replace it; a confirm-only step never does.
        let saved = record
            .filter(|_| first_order && pair_destination.is_none())
            .and_then(|record| record.swap_use(swap_use)?.approval());
        // Each setup's fee limit was approved with that setup, and stays bound. The accounts
        // are bound as the swap use holds them now, so a review approved after a changed
        // account names the current ones.
        let destination_setup_fee = saved.and_then(|saved| saved.bounds.destination_setup_fee);
        let source_setup_fee = saved.and_then(|saved| saved.bounds.source_setup_fee);
        let accounts = record
            .and_then(|record| self.bound_accounts(record, swap_use))
            .or_else(|| saved.and_then(|saved| saved.accounts));
        let replacement = (approval.full_review && saved.is_some()).then(|| {
            approval
                .review
                .approval(approval.private_minimum, approval.price_acknowledged)
                .map(|mut replacement| {
                    replacement.bounds.destination_setup_fee = destination_setup_fee;
                    replacement.bounds.source_setup_fee = source_setup_fee;
                    replacement.accounts = accounts;
                    replacement
                })
        });
        let pending = crate::root::private_swap::PendingSwapOrder {
            // The draft waits for the next order of its own swap use.
            previous_order: self
                .record(operation)
                .and_then(|record| {
                    crate::root::private_swap::model::swap_use_last_order(record, swap_use)
                })
                .map(wallet_ops::vault::SwapOrderRecord::uid),
            sell: plan.sell_token(),
            buy: plan.buy_token(),
            delivery: plan.delivery(),
            amount: plan.amount(),
            private_minimum: approval.private_minimum,
            slippage_bps: approval.review.slippage_bps(),
            gas_share_bps: approval.review.gas_share_bps(),
            valid_for: approval.review.valid_for(),
            reuse_account: plan.swap_executor().is_reused(),
            swap_use,
            started_at: now_unix(),
            speeds_up: speed_up.is_some(),
        };
        self.tracking.entry(operation).or_default().pending_order = Some(pending);
        self.reapproval = None;
        self.start_job(
            operation,
            SwapJobKind::Order,
            async move {
                if let Some(replacement) = replacement {
                    owner.record_swap_approval(operation, swap_use, replacement?)?;
                }
                if let (Some(account), Some(destination)) = (pair_destination, &destination) {
                    Box::pin(prepare_swap_pair(
                        &owner,
                        Some(destination.owner.as_ref()),
                        SwapPairPreparation {
                            use_id: swap_use,
                            source: SwapAccountChoice::Existing(operation),
                            destination: Some(SwapAccountChoice::Existing(account.operation)),
                            approval: approval
                                .review
                                .approval(approval.private_minimum, approval.price_acknowledged)?,
                            candidate: None,
                            destination_candidate: None,
                            authorization: &authorization,
                            destination_authorization: Some(&destination.authorization),
                            client: approval.orderbook.http(),
                        },
                    ))
                    .await?;
                }
                Box::pin(owner.submit_swap_order(SwapOrderRequest {
                    review: approval.review.as_ref(),
                    swap_use,
                    private_minimum: approval.private_minimum,
                    price_acknowledged: approval.price_acknowledged,
                    session,
                    authorization,
                    orderbook: &approval.orderbook,
                    anchor_cache: &anchors,
                    token_registry: &tokens,
                    bridge: approval.bridge.as_ref().map(QuotedBridge::route),
                    destination_minimum: approval.destination_minimum,
                    destination,
                    continuing_minimum: approval.continuing_minimum,
                    verify_proof: true,
                }))
                .await
            },
            move |this, outcome, window, cx| {
                if speed_up.is_some() {
                    this.finish_speed_up(operation, outcome, window, cx);
                } else {
                    this.finish_order(operation, outcome, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Continue after a speed-up's order job. A placed order shows in the swap's detail, as any
    /// order does. Changed terms go back to the speed-up's own form with the change named, and
    /// never to a retry or a new swap: the open order is still live, and only the owner's rule
    /// says whether it can still be sped up.
    fn finish_speed_up(
        &mut self,
        operation: ExecutorOperationId,
        outcome: SwapOrderOutcome,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !matches!(outcome, SwapOrderOutcome::Submitted { .. }) {
            // No order was placed, so the open one is the swap's order again.
            self.tracking.entry(operation).or_default().pending_order = None;
        }
        let change = match outcome {
            SwapOrderOutcome::Submitted { .. } => {
                self.finish_order(operation, outcome, window, cx);
                return;
            }
            SwapOrderOutcome::ReviewRequired(change) => change,
            SwapOrderOutcome::Replan { byte_budget, .. } => {
                self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                self.fail(
                    operation,
                    "The orderbook rejected the new order's size, so this order can't be sped up."
                        .into(),
                );
                cx.notify();
                return;
            }
        };
        self.track_delivery_shortfall(operation, outcome);
        let review_again = format!(
            "Review the speed-up again: {}.",
            review_change_label(change)
        );
        // Another dialog keeps the screen. The change waits for the speed-up to be opened again.
        if window.has_active_dialog(cx) && !self.swap_dialog_shows(operation, window, cx) {
            self.reapproval = Some((operation, change));
            self.fail(operation, review_again);
            cx.notify();
            return;
        }
        // Nothing was signed. Quote the current terms, then review them again.
        self.reapproval = Some((operation, change));
        if !self.open_speed_up_form(operation, window, cx) {
            self.reapproval = None;
            self.fail(
                operation,
                "This order can no longer be sped up. Nothing was signed.".into(),
            );
        }
        cx.notify();
    }

    /// A signing quote below the approved destination minimum shows what the preview's
    /// delivery allowance lacks. Record it before the swap's next review is quoted.
    fn track_delivery_shortfall(
        &mut self,
        operation: ExecutorOperationId,
        outcome: SwapOrderOutcome,
    ) {
        if let SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved,
            current,
        }) = outcome
        {
            let shortfall = &mut self
                .tracking
                .entry(operation)
                .or_default()
                .delivery_shortfall;
            *shortfall = Some(
                shortfall
                    .unwrap_or_default()
                    .saturating_add(delivery_shortfall_allowance(approved, current)),
            );
        }
    }

    /// The accounts the swap use `swap_use` of `record` holds, as an approval binds them: each
    /// with its address and whether the use set it up. `None` while the use isn't the
    /// record's, or its destination account's network isn't loaded.
    pub(super) fn bound_accounts(
        &self,
        record: &ExecutorRecord,
        swap_use: SwapUseId,
    ) -> Option<SwapApprovedAccounts> {
        let bound = |account: &ExecutorRecord| {
            Some(SwapApprovedAccount {
                address: account.address(),
                setup: account.swap_use(swap_use).map(SwapUseRecord::is_fresh)?,
            })
        };
        let destination =
            match crate::root::private_swap::model::swap_use_destination(record, swap_use) {
                Some((delivery, _)) => {
                    let swap = crate::root::private_swap::model::SwapIdentity {
                        operation: record.operation(),
                        swap_use,
                    };
                    Some(bound(self.swap_destination_account(swap, delivery)?)?)
                }
                None => None,
            };
        Some(SwapApprovedAccounts {
            source: bound(record)?,
            destination,
        })
    }

    pub(in crate::root::private_swap) fn finish_order(
        &mut self,
        operation: ExecutorOperationId,
        outcome: SwapOrderOutcome,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = self.swap_dialog_shows(operation, window, cx);
        if let SwapOrderOutcome::Submitted { .. } = outcome {
            let tracking = self.tracking.entry(operation).or_default();
            tracking.cursor = None;
            tracking.error = None;
            tracking.auto_place = false;
            tracking.delivery_shortfall = None;
            // The swap's own form or detail moves on to the detail; another dialog stays.
            if !window.has_active_dialog(cx) || shown {
                self.show_detail(operation, window, cx);
            }
            cx.notify();
            return;
        }
        self.track_delivery_shortfall(operation, outcome);
        if window.has_active_dialog(cx)
            && !shown
            && self
                .form
                .as_ref()
                .is_none_or(|form| form.operation != Some(operation))
        {
            match outcome {
                SwapOrderOutcome::ReviewRequired(change) => {
                    self.reapproval = Some((operation, change));
                    self.fail(
                        operation,
                        format!("Review the swap again: {}.", review_change_label(change)),
                    );
                }
                SwapOrderOutcome::Replan { byte_budget, .. } => {
                    self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                    self.fail(operation, "The order is too large. Retry with a smaller amount after this attempt ends.".into());
                }
                SwapOrderOutcome::Submitted { .. } => {}
            }
            cx.notify();
            return;
        }
        // An order placed from the detail continues in the form.
        if self
            .form
            .as_ref()
            .is_none_or(|form| form.operation != Some(operation))
        {
            self.open_existing_form(operation, window, cx);
        }
        match outcome {
            SwapOrderOutcome::Submitted { .. } => {}
            SwapOrderOutcome::ReviewRequired(change) => {
                // Nothing was signed. Quote the current terms, then review them again.
                self.reapproval = Some((operation, change));
                self.schedule_quote(window, cx);
            }
            SwapOrderOutcome::Replan {
                byte_budget,
                attempt_recorded,
            } => {
                self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                if let Some(form) = self.form.as_mut() {
                    form.quote = QuoteState::Idle;
                    form.quote_details_open = false;
                    form.error = Some(if attempt_recorded {
                        "The orderbook rejected this order's size. The order stays recorded until it expires. Then retry, and the swap offers the largest amount that fits."
                    } else {
                        "The order is too large for the orderbook. Enter a smaller amount."
                    }
                    .into());
                }
                if !attempt_recorded {
                    self.schedule_quote(window, cx);
                }
            }
        }
        cx.notify();
    }
}
