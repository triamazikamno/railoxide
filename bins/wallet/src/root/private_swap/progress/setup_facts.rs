use super::*;

impl PrivateSwapsView {
    /// Each setup's steps, under its network and stealth account when the swap has two
    /// accounts. Two setups sit side by side, or one above the other when `stacked`. A setup
    /// that `retries` offers to send again has Retry setup… on the row it failed or waits at,
    /// which opens the retry of that account alone. A destination account that was retired
    /// has Retry with new account… there, as the footer has for the swap's own.
    pub(super) fn render_setups(
        &self,
        operation: ExecutorOperationId,
        setups: &[SetupColumn],
        retries: &[SetupRetryAvailability],
        stacked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let busy = self.busy();
        let stepper = |(setup, retry): (&SetupColumn, &SetupRetryAvailability)| {
            let side = setup.side;
            let id = match side {
                SetupSide::Origin => "swap-setup-retry-origin",
                SetupSide::Destination => "swap-setup-retry-destination",
            };
            let new_account =
                side == SetupSide::Destination && self.retries_with_new_account(operation, side);
            // Its own job runs while it waits, and stops for the retry.
            let outlasted = self.setup_wait_outlasted(operation, side);
            let retry =
                retry
                    .clone()
                    .or_else(|| new_account.then_some(Ok(())))
                    .map(|availability| {
                        div()
                            .pt(px(4.0))
                            .flex()
                            .flex_col()
                            .items_start()
                            .child(
                                app_button(
                                    id,
                                    if new_account {
                                        "Retry with new account…"
                                    } else {
                                        "Retry setup…"
                                    },
                                )
                                .debug_selector(move || id.into())
                                .small()
                                .outline()
                                .flex_none()
                                .disabled((busy && !outlasted) || availability.is_err())
                                .when_some(
                                    availability.err(),
                                    gpui_component::button::Button::tooltip,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_setup_retry(operation, side, window, cx);
                                    },
                                )),
                            )
                            .into_any_element()
                    });
            let stepper = render_submission_progress_stepper(setup_steps(
                operation,
                setup,
                self.shown_setup_progress(operation, side),
                retry,
            ));
            match self.render_setup_broadcaster(operation, side, cx) {
                Some(broadcaster) => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(stepper)
                    .child(broadcaster),
                None => stepper,
            }
        };
        let mut steppers = setups.iter().zip(retries).map(stepper);
        if let [setup] = setups
            && setup.heading.is_none()
            && let Some(stepper) = steppers.next()
        {
            return stepper;
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .gap_3()
            .when(stacked, |row| row.flex_col().items_stretch())
            .debug_selector(|| "swap-setup-columns".into())
            .children(setups.iter().zip(steppers).map(|(setup, stepper)| {
                let selector = match setup.side {
                    SetupSide::Origin => "swap-setup-column-origin",
                    SetupSide::Destination => "swap-setup-column-destination",
                };
                div()
                    .min_w_0()
                    .map(|column| {
                        if stacked {
                            column.w_full()
                        } else {
                            column.flex_1()
                        }
                    })
                    .flex()
                    .flex_col()
                    .gap_2()
                    .debug_selector(move || selector.into())
                    .children(setup.heading.as_ref().map(|(network, account)| {
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(app_strong_text(network.clone()).flex_none())
                            .children(
                                account
                                    .map(|account| stealth_account("swap-step-account", account)),
                            )
                    }))
                    .child(stepper)
            }))
    }

    /// The Broadcaster row under `side`'s stages: the broadcaster this session sent the setup
    /// to, with its preference state, and Ban once the setup's wait outlasted its re-sends.
    /// A setup without this session's stages has none.
    fn render_setup_broadcaster(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        use crate::root::private_broadcaster::BroadcasterPreferenceControl::{
            AddFavorite, Ban, Banned, Favorited,
        };

        let progress = self.shown_setup_progress(operation, side)?;
        let broadcaster = progress.broadcaster();
        let address = broadcaster.railgun_address.as_str();
        let control = {
            let root = self.root.upgrade()?;
            let root = root.read(cx);
            broadcaster_preference_control(
                root.is_banned_broadcaster(address),
                root.is_favorite_broadcaster(address),
                progress.end() == Some(SetupProgressEnd::Submitted),
                progress.wait_outlasted(),
            )
        };
        let side_id = match side {
            SetupSide::Origin => "origin",
            SetupSide::Destination => "destination",
        };
        let action = control.map(|control| {
            let state = match control {
                Banned => "banned",
                Favorited => "favorited",
                AddFavorite => "favorite",
                Ban => "ban",
            };
            let view = cx.weak_entity();
            div()
                .flex_none()
                .debug_selector(move || format!("swap-setup-broadcaster-{state}-{side_id}"))
                .child(render_broadcaster_preference_control(
                    control,
                    SharedString::from(format!(
                        "swap-setup-broadcaster-{}-{side_id}",
                        operation.opaque_id()
                    )),
                    address,
                    self.root.clone(),
                    move |cx| {
                        let _ = view.update(cx, |_, cx| cx.notify());
                    },
                ))
                .into_any_element()
        });
        Some(
            private_broadcaster_context_row_with_action(
                "Broadcaster",
                broadcaster_candidate_label(broadcaster),
                action,
            )
            .debug_selector(move || format!("swap-setup-broadcaster-{side_id}")),
        )
    }

    /// The stealth accounts of a swap that set one up, as facts: the swap's own, or for a
    /// private Bridge swap each account that is confirmed or reused, with its network. While
    /// the record leaves the swap's own setup unresolved, its fact also tells that setup, as
    /// [`Self::own_setup_fact`] does.
    pub(super) fn account_facts(
        &self,
        record: &ExecutorRecord,
        labels: &SwapLabels,
        stage: SwapStage,
        cx: &Context<'_, Self>,
    ) -> Vec<gpui::Div> {
        let private = labels
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.private.as_ref());
        let Some(private) = private else {
            return record
                .address()
                .map(|address| {
                    let fact = fact_row(
                        "Stealth account",
                        stealth_account(
                            "swap-fact-account",
                            SwapStepAccount {
                                index: Some(record.index()),
                                address,
                            },
                        ),
                    )
                    .debug_selector(|| "swap-detail-account".into());
                    self.with_own_setup_fact(fact, record, cx)
                })
                .into_iter()
                .collect();
        };
        [SetupSide::Origin, SetupSide::Destination]
            .into_iter()
            .zip(&private.setups)
            .filter(|(side, account)| {
                account.progress == SwapSetupProgress::Done
                    // Held proceeds name the destination account among the bridge's facts.
                    && (*side == SetupSide::Origin || !stage.is_held_on_destination())
            })
            .filter_map(|(side, account)| {
                let fact = fact_row(
                    format!("Stealth account on {}", account.network),
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(stealth_account("swap-fact-account", account.account?))
                        .child(app_muted_text(if account.reused {
                            "· reused"
                        } else {
                            "· set up"
                        })),
                )
                .debug_selector(|| "swap-detail-account".into());
                Some(match side {
                    SetupSide::Origin => self.with_own_setup_fact(fact, record, cx),
                    SetupSide::Destination => fact,
                })
            })
            .collect()
    }

    /// `fact`, the swap's own stealth account, with [`Self::own_setup_fact`] under it.
    fn with_own_setup_fact(
        &self,
        fact: gpui::Div,
        record: &ExecutorRecord,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        match self.own_setup_fact(record, cx) {
            Some(setup) => div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .child(fact)
                .child(setup),
            None => fact,
        }
    }

    /// What the swap's own stealth account fact tells of its setup while the record leaves
    /// that setup unresolved and the detail is past it: the confirmation the setup's own body
    /// shows while private sync or this session's latest read shows it included, until it is
    /// confirmed. Once neither shows it under an order, open or ended, that the setup is no
    /// longer included, with Retry setup…, which sends it again with the same account.
    fn own_setup_fact(&self, record: &ExecutorRecord, cx: &Context<'_, Self>) -> Option<gpui::Div> {
        wallet_ops::own_pending_swap_setup(record)?;
        // Before the order, the setup's own body tells it until the latest read shows it
        // included.
        if record.swap().is_none() && self.setup_inclusion(record).is_none() {
            return None;
        }
        if self.own_setup_included(record) {
            let confirmation = self
                .setup_confirmation_detail(record, cx)
                .unwrap_or_else(|| "Confirming…".into());
            return Some(
                div()
                    .debug_selector(|| "swap-detail-account-setup".into())
                    .child(app_muted_shimmer_text(confirmation).whitespace_normal()),
            );
        }
        record.swap()?;
        let operation = record.operation();
        let busy = self.busy();
        let retry = self
            .setup_retry_availability(record, SetupSide::Origin, cx)
            .map(|availability| {
                app_button("swap-setup-retry-origin", "Retry setup…")
                    .debug_selector(|| "swap-setup-retry-origin".into())
                    .small()
                    .outline()
                    .flex_none()
                    .disabled(busy || availability.is_err())
                    .when_some(availability.err(), gpui_component::button::Button::tooltip)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_setup_retry(operation, SetupSide::Origin, window, cx);
                    }))
            });
        Some(
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .debug_selector(|| "swap-detail-account-setup".into())
                .child(app_muted_text("The setup is no longer included.").whitespace_normal())
                .children(retry),
        )
    }

    /// An earlier swap on a reused stealth account, read only: its steps, facts and outcome from
    /// its own orders, and a footer that only closes. The account's latest swap owns every action.
    pub(in crate::root::private_swap) fn render_past_detail(
        &self,
        swap: SwapIdentity,
        first: usize,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let close = app_button("swap-progress-close", "Close")
            .small()
            .flex_none()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_swap_dialog(window, cx);
            }));
        let footer = div()
            .w_full()
            .flex()
            .items_center()
            .child(div().flex_1())
            .child(close);
        let Some((record, past, order)) = self.past_swap(swap, first) else {
            return (
                app_muted_text("This swap is no longer saved in this wallet."),
                Some(footer),
            );
        };
        let attempts = SwapAttempts::ending_at(record, order);
        let stage = attempts.stage(record, self.attribution(record));
        let labels = self.past_labels(record, order, cx);
        let steps = swap_steps(stage, &labels)
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                let detail = step.detail.clone();
                progress_group(
                    &step,
                    detail,
                    format!("swap-step-{}-{first}-{index}", swap.operation.opaque_id()),
                    None,
                )
            });
        // Recovery acts on the account, which its latest swap and Stealth accounts offer.
        let recovery = (stage.needs_recovery() || self.stage(record).needs_recovery()).then_some(
            "This stealth account needs recovery. Recover it from its latest swap or from Stealth accounts.",
        );
        let started = self.swap_started(record, Some(&past), cx);
        let outcome = self.render_outcome(record, attempts.outcome, stage, started, cx);
        let not_filled = self.render_not_filled(record, attempts.outcome, stage, cx);
        let facts = match order.delivery() {
            SwapDelivery::Bridge(bridge) => {
                Some(self.render_bridge_facts(record, Some(order), bridge, stage, started, cx))
            }
            SwapDelivery::NonEvmBridge(non_evm) => {
                Some(self.render_non_evm_facts(record, Some(order), &non_evm, stage, started, cx))
            }
            delivery if outcome.is_none() => {
                self.render_facts(Some(order), &delivery, None, started, cx)
            }
            _ => None,
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .children(
                shows_steps(stage, outcome.is_some())
                    .then(|| render_submission_progress_groups(steps)),
            )
            .children(not_filled)
            .children(facts)
            .children(outcome)
            .children(self.render_attempts(record, &attempts, cx))
            .children(
                earlier_attempts_note(&attempts)
                    .map(|note| app_muted_text(note).whitespace_normal()),
            )
            .children(recovery.map(|note| app_muted_text(note).whitespace_normal()));
        (body, Some(footer))
    }

    /// Who settles the order, a Public address swap's receiver, the minimum while it can fill,
    /// and when the swap started. The order step shows the order ID.
    pub(super) fn render_facts(
        &self,
        order: Option<&SwapOrderRecord>,
        delivery: &SwapDelivery,
        minimum: Option<(Address, U256)>,
        started: Option<(&'static str, u64)>,
        cx: &App,
    ) -> Option<gpui::Div> {
        let mut rows = Vec::new();
        if order.is_some() {
            rows.push(fact_row("Settled by", settled_by_cow()));
        }
        if let &SwapDelivery::External { receiver } = delivery {
            rows.push(self.receiver_row(receiver, cx));
        }
        if let Some((buy, minimum)) = minimum {
            rows.push(fact_row(
                "Receive at least",
                app_text(self.with_usd(self.token_amount(buy, minimum, cx), buy, minimum, cx)),
            ));
        }
        if let Some((label, at)) = started {
            rows.push(fact_row(label, app_text(local_date_time_label(at))));
        }
        (!rows.is_empty()).then(|| div().w_full().flex().flex_col().gap_2().children(rows))
    }

    /// A Public address swap's receiver as [`receiver_fact_row`] lays it out: the wallet's
    /// label for it when it has one, and its address, shortened, with a control that copies it
    /// in full.
    pub(super) fn receiver_row(&self, receiver: Address, cx: &App) -> gpui::Div {
        receiver_fact_row(
            receiver.to_checksum(None),
            short_receiver(receiver),
            self.receiver_label(receiver, cx).map(|(label, _)| label),
        )
    }

    /// A Bridge swap's facts: its receiver, destination and provider, then what its state
    /// needs, from the provider's deposit and the destination outcome to the settlement on this
    /// network. A deposit that needs attention shows its address in full. A private delivery
    /// has no receiver: its destination is the private balance on the destination network, and
    /// held proceeds name the destination stealth account. `order` is the swap's latest. The
    /// deposit, the destination outcome, the minimum a delivery is held to, a refund and the
    /// surplus come from the attempt that delivered, and the settlement from the one that
    /// traded.
    pub(super) fn render_bridge_facts(
        &self,
        record: &ExecutorRecord,
        order: Option<&SwapOrderRecord>,
        delivery: BridgeDelivery,
        stage: SwapStage,
        started: Option<(&'static str, u64)>,
        cx: &App,
    ) -> gpui::Div {
        let network = network_name(delivery.destination_chain);
        let origin = network_name(self.origin_chain_id);
        let provider = provider_name(delivery.provider);
        let private = delivery.is_private();
        let destination_amount = |amount| {
            self.network_token_amount(
                delivery.destination_chain,
                self.delivered_token(delivery, cx),
                amount,
                cx,
            )
        };
        let buy = order
            .and_then(|order| Some(record.swap()?.order_terms(order).buy_token()))
            .or_else(|| swap_tokens(record).map(|(_, buy)| buy));
        let attempts = order.map(|order| SwapAttempts::ending_at(record, order));
        let evidence = attempts.map(|attempts| attempts.bridge_evidence());
        let observed = evidence
            .map(SwapOrderRecord::observations)
            .unwrap_or_default();
        let terms = evidence.and_then(SwapOrderRecord::bridge);
        let (across, near) = match terms {
            Some(BridgeOrderTerms::Across(across)) => (Some(across), None),
            Some(BridgeOrderTerms::NearIntents(near)) => (None, Some(near)),
            None => (None, None),
        };
        let bounds = evidence
            .map(SwapOrderRecord::bounds)
            .or_else(|| record.swap_approval().map(|approval| &approval.bounds));
        // The private balance gets the minimum less the destination network's shield fee.
        let minimum = bounds.and_then(|bounds| {
            let minimum = bounds.destination_minimum?;
            Some(if private {
                private_delivery_credit(minimum, bounds)
            } else {
                minimum
            })
        });
        let receiver = (!private).then(|| self.receiver_row(delivery.receiver, cx));
        let destination = fact_row(
            "Destination",
            destination_value(delivery.destination_chain, private),
        );
        let provider_row = fact_row("Provider", provider_mention(delivery.provider, app_text));
        let deposit = observed
            .bridge_handoff
            .and_then(|handoff| handoff.deposit_id)
            .map(|id| {
                let id = id.to_string();
                let copy_id = SharedString::from(format!("swap-deposit-{id}-copy"));
                hash_row("Deposit ID", id, copy_id, "Copy Across deposit ID")
            })
            .or_else(|| near.map(|near| near_deposit_row(near.deposit_address)));
        let settlement = attempts
            .and_then(|attempts| attempts.traded)
            .and_then(|traded| traded.observations().traded?.transaction_hash)
            .map(|hash| {
                let hash = hash.to_string();
                let copy_id = SharedString::from(format!("swap-settlement-{hash}-copy"));
                hash_row(
                    "Settlement",
                    hash,
                    copy_id,
                    "Copy settlement transaction hash",
                )
            });
        let started =
            started.map(|(label, at)| fact_row(label, app_text(local_date_time_label(at))));
        // What this session's last explicit check found in the stealth account.
        let checked = buy
            .zip(
                self.tracking
                    .get(&record.operation())
                    .and_then(|tracking| tracking.stealth_balance)
                    .map(|(balance, _)| balance),
            )
            .map(|(buy, balance)| {
                fact_row(
                    "In stealth account",
                    app_text(self.token_amount(buy, balance, cx)),
                )
            });
        let earlier_minimum = attempts
            .filter(|_| stage == SwapStage::Order(SwapOrderState::Done))
            .and_then(|attempts| self.earlier_minimum_note(&attempts, delivery, cx));
        let mut rows = Vec::new();
        let mut note = None;
        match stage {
            SwapStage::Order(SwapOrderState::Done) => {
                rows.extend(receiver);
                match observed.bridge_outcome {
                    Some(SwapBridgeOutcome::DeliveredVerified {
                        block,
                        output_amount,
                        shielded,
                        ..
                    }) => {
                        if private {
                            // What the destination stealth account shielded, less the fee.
                            let received =
                                bounds.filter(|_| shielded).map_or(output_amount, |bounds| {
                                    private_delivery_credit(output_amount, bounds)
                                });
                            rows.push(fact_row(
                                "Received",
                                amount_with_note(
                                    destination_amount(received),
                                    minimum.map_or_else(String::new, |minimum| {
                                        format!("(minimum {})", destination_amount(minimum))
                                    }),
                                ),
                            ));
                            rows.push(destination);
                        } else {
                            rows.push(fact_row(
                                "Delivered",
                                amount_with_note(
                                    destination_amount(output_amount),
                                    format!("on {network}"),
                                ),
                            ));
                        }
                        // A post-hook that reshields the surplus credits it privately.
                        rows.extend(buy.zip(observed.settlement_credit).map(|(buy, credit)| {
                            fact_row(
                                "Surplus",
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_wrap()
                                    .justify_end()
                                    .gap_1()
                                    .child(
                                        app_text(format!(
                                            "+{}",
                                            self.token_amount(buy, credit.private_amount, cx)
                                        ))
                                        .text_color(cx.theme().success),
                                    )
                                    .child(app_muted_text(format!("reshielded on {origin}"))),
                            )
                        }));
                        rows.push(provider_row);
                        // A fact, not a link: looking the block up is the user's choice.
                        rows.push(fact_row(
                            "Fill",
                            app_text(format!(
                                "{network} block {}",
                                railgun_ui::format_token_amount(U256::from(block.number), 0)
                            )),
                        ));
                        if !private {
                            rows.push(fact_row("Settled by", settled_by_cow()));
                        }
                    }
                    Some(SwapBridgeOutcome::DeliveredReported { amount_out, .. }) => {
                        rows.extend(amount_out.map(|amount| {
                            fact_row(
                                "Reported amount",
                                amount_with_note(
                                    destination_amount(amount),
                                    minimum.map_or_else(String::new, |minimum| {
                                        format!("(minimum {})", destination_amount(minimum))
                                    }),
                                ),
                            )
                        }));
                        rows.push(provider_row);
                        rows.extend(deposit);
                        note = Some(match delivery.provider {
                            BridgeProvider::Across => format!(
                                "{provider} reports this delivery. The wallet couldn't verify it on {network}."
                            ),
                            BridgeProvider::NearIntents => format!(
                                "{provider} reports this delivery. The wallet doesn't check it on {network}, because looking the transaction up would tell the RPC provider which swap is yours."
                            ),
                        });
                    }
                    _ => rows.push(provider_row),
                }
                rows.extend(settlement);
            }
            SwapStage::Order(SwapOrderState::Refunding) => {
                rows.extend(receiver);
                rows.push(provider_row);
                rows.extend(deposit);
                rows.extend(across.map(|across| {
                    fact_row(
                        "Deposit expired",
                        app_text(local_date_time_label(u64::from(across.fill_deadline))),
                    )
                }));
                rows.extend(checked);
                note = Some(
                    "Check status confirms the refund reached the stealth account. Then Recover… shields it to your private balance for the shield fee and a broadcaster fee."
                        .to_owned(),
                );
            }
            SwapStage::Order(SwapOrderState::NeedsAttention) => {
                rows.extend(near.map(|near| deposit_address_box(near.deposit_address)));
                rows.extend(
                    buy.zip(evidence.and_then(bridge_sent_amount))
                        .map(|(buy, sent)| {
                            fact_row("Amount sent", app_text(self.token_amount(buy, sent, cx)))
                        }),
                );
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(settlement);
                note = Some(format!(
                    "Search the deposit address on explorer.near-intents.org to see where the swap is, and give {provider} support the address. If 1Click refunds it, the {} goes to the stealth account on {origin}, and Check status finds it for recovery.",
                    buy.map_or_else(|| "deposit".to_owned(), |buy| self.token_symbol(buy, cx))
                ));
            }
            // The Across post-hook didn't run, so there's no deposit.
            SwapStage::Order(SwapOrderState::NotDelivered) => {
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(settlement);
                rows.extend(checked);
            }
            // The fill completed without its shield: the destination stealth account, the
            // fill, and what this session's last explicit check found in the account.
            SwapStage::Order(SwapOrderState::HeldOnDestination) => {
                rows.push(fact_row(
                    format!("Account on {network}"),
                    stealth_account(
                        "swap-held-account",
                        SwapStepAccount {
                            index: self
                                .order_swap(record, order)
                                .and_then(|swap| self.swap_destination_account(swap, delivery))
                                .map(ExecutorRecord::index),
                            address: delivery.receiver,
                        },
                    ),
                ));
                rows.push(provider_row);
                if let Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) =
                    observed.bridge_outcome
                {
                    rows.push(fact_row(
                        "Fill",
                        app_text(format!(
                            "{network} block {}",
                            railgun_ui::format_token_amount(U256::from(block.number), 0)
                        )),
                    ));
                }
                rows.extend(self.destination_balance(record).map(|(balance, _)| {
                    fact_row("In stealth account", app_text(destination_amount(balance)))
                }));
                note = Some(format!(
                    "Recover on {network}… shields it to your private balance on {network} for the shield fee and a broadcaster fee, paid from your private balance there."
                ));
            }
            SwapStage::Order(SwapOrderState::Traded | SwapOrderState::Bridging) => {
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(deposit);
                rows.extend(settlement);
                rows.extend(started);
            }
            // Before the trade: who settles the order, and what the receiver gets on the
            // destination network.
            _ => {
                if order.is_some() {
                    rows.push(fact_row("Settled by", settled_by_cow()));
                }
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                // Across delivers its exact output; NEAR Intents at least its minimum.
                rows.extend(
                    minimum
                        .filter(|_| swap_order_group(stage, false, false) == SwapOrderGroup::Open)
                        .map(|minimum| {
                            let amount = destination_amount(minimum);
                            fact_row(
                                format!("Receive on {network}"),
                                app_text(match delivery.provider {
                                    BridgeProvider::Across => amount,
                                    BridgeProvider::NearIntents => format!("at least {amount}"),
                                }),
                            )
                        }),
                );
                rows.extend(started);
            }
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "swap-bridge-facts".into())
            .child(div().w_full().flex().flex_col().gap_2().children(rows))
            .children(earlier_minimum.map(|note| {
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "swap-bridge-earlier-minimum".into())
                    .child(app_muted_text(note).whitespace_normal())
            }))
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
    }

    /// The facts of a swap NEAR Intents delivers to a non-EVM network, from its record alone,
    /// as [`Self::render_bridge_facts`] has them for a chain: the receiver in its network's form,
    /// the network, the provider, the deposit address, the settlement, and what 1Click reports,
    /// with amounts in the record's token and decimals and the destination transaction in the
    /// network's own form. The wallet has no connection to the network, so nothing there is
    /// verified.
    pub(super) fn render_non_evm_facts(
        &self,
        record: &ExecutorRecord,
        order: Option<&SwapOrderRecord>,
        delivery: &NonEvmDelivery,
        stage: SwapStage,
        started: Option<(&'static str, u64)>,
        cx: &App,
    ) -> gpui::Div {
        let network = delivery.network.display_name();
        let origin = network_name(self.origin_chain_id);
        let provider = provider_name(BridgeProvider::NearIntents);
        let amount = |amount| non_evm_amount(delivery, amount);
        let buy = order
            .and_then(|order| Some(record.swap()?.order_terms(order).buy_token()))
            .or_else(|| swap_tokens(record).map(|(_, buy)| buy));
        let attempts = order.map(|order| SwapAttempts::ending_at(record, order));
        let evidence = attempts.map(|attempts| attempts.bridge_evidence());
        let observed = evidence
            .map(SwapOrderRecord::observations)
            .unwrap_or_default();
        let near = match evidence.and_then(SwapOrderRecord::bridge) {
            Some(BridgeOrderTerms::NearIntents(near)) => Some(near),
            _ => None,
        };
        let minimum = evidence
            .map(SwapOrderRecord::bounds)
            .or_else(|| record.swap_approval().map(|approval| &approval.bounds))
            .and_then(|bounds| bounds.destination_minimum);
        let receiver = self.non_evm_receiver_row(delivery, cx);
        let destination = fact_row(
            "Destination",
            div()
                .flex()
                .items_center()
                .text_size(theme::APP_TEXT_SIZE)
                .line_height(gpui::relative(theme::APP_TEXT_LINE_HEIGHT))
                .child(
                    gpui::img(delivery.network.icon_asset_path())
                        .size(px(16.0))
                        .flex_none()
                        .mr_1(),
                )
                .child(network),
        );
        let provider_row = fact_row(
            "Provider",
            provider_mention(BridgeProvider::NearIntents, app_text),
        );
        let deposit = near.map(|near| near_deposit_row(near.deposit_address));
        let settlement = attempts
            .and_then(|attempts| attempts.traded)
            .and_then(|traded| traded.observations().traded?.transaction_hash)
            .map(|hash| {
                let hash = hash.to_string();
                let copy_id = SharedString::from(format!("swap-settlement-{hash}-copy"));
                hash_row(
                    "Settlement",
                    hash,
                    copy_id,
                    "Copy settlement transaction hash",
                )
            });
        let started =
            started.map(|(label, at)| fact_row(label, app_text(local_date_time_label(at))));
        let mut rows = Vec::new();
        let mut note = None;
        match stage {
            SwapStage::Order(SwapOrderState::Done) => {
                rows.push(receiver);
                if let Some(SwapBridgeOutcome::DeliveredReported {
                    amount_out,
                    destination_transaction,
                    ..
                }) = &observed.bridge_outcome
                {
                    rows.extend(amount_out.map(|reported| {
                        fact_row(
                            "Reported amount",
                            amount_with_note(
                                amount(reported),
                                minimum.map_or_else(String::new, |minimum| {
                                    format!("(minimum {})", amount(minimum))
                                }),
                            ),
                        )
                    }));
                    rows.extend(destination_transaction.clone().map(|transaction| {
                        let copy_id = SharedString::from(format!(
                            "swap-destination-transaction-{transaction}-copy"
                        ));
                        fact_row(
                            "Transaction",
                            div()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap_1()
                                .debug_selector(|| "swap-destination-transaction".into())
                                .child(
                                    app_strong_text(railgun_ui::short_non_evm(&transaction))
                                        .flex_none()
                                        .font_family(theme::APP_MONO_FONT_FAMILY),
                                )
                                .child(
                                    clipboard_with_toast(copy_id, transaction)
                                        .tooltip("Copy transaction"),
                                ),
                        )
                    }));
                }
                rows.push(provider_row);
                rows.extend(deposit);
                rows.extend(settlement);
                note = Some(format!(
                    "{provider} reports this delivery. The wallet has no connection to {network}, so it doesn't check it there."
                ));
            }
            SwapStage::Order(SwapOrderState::Refunding) => {
                rows.push(receiver);
                rows.push(provider_row);
                rows.extend(deposit);
                note = Some(format!(
                    "{provider} refunded the deposit to the stealth account on {origin}. Check status confirms the refund reached it. Then Recover… shields it to your private balance for the shield fee and a broadcaster fee."
                ));
            }
            SwapStage::Order(SwapOrderState::NeedsAttention) => {
                rows.extend(near.map(|near| deposit_address_box(near.deposit_address)));
                rows.extend(
                    buy.zip(evidence.and_then(bridge_sent_amount))
                        .map(|(buy, sent)| {
                            fact_row("Amount sent", app_text(self.token_amount(buy, sent, cx)))
                        }),
                );
                rows.extend([receiver, destination, provider_row]);
                rows.extend(settlement);
                note = Some(format!(
                    "Search the deposit address on explorer.near-intents.org to see where the swap is, and give {provider} support the address. If 1Click refunds it, the {} goes to the stealth account on {origin}, and Check status finds it for recovery.",
                    buy.map_or_else(|| "deposit".to_owned(), |buy| self.token_symbol(buy, cx))
                ));
            }
            SwapStage::Order(SwapOrderState::Traded | SwapOrderState::Bridging) => {
                rows.extend([receiver, destination, provider_row]);
                rows.extend(deposit);
                rows.extend(settlement);
                rows.extend(started);
            }
            // Before the trade: who settles the order, and what the receiver gets there.
            _ => {
                if order.is_some() {
                    rows.push(fact_row("Settled by", settled_by_cow()));
                }
                rows.extend([receiver, destination, provider_row]);
                rows.extend(
                    minimum
                        .filter(|_| swap_order_group(stage, false, false) == SwapOrderGroup::Open)
                        .map(|minimum| {
                            fact_row(
                                format!("Receive on {network}"),
                                app_text(format!("at least {}", amount(minimum))),
                            )
                        }),
                );
                rows.extend(started);
            }
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "swap-bridge-facts".into())
            .child(div().w_full().flex().flex_col().gap_2().children(rows))
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
    }

    /// A non-EVM receiver as [`receiver_fact_row`] lays it out: its saved label on that
    /// network, and the receiver shortened with a control that copies it in full.
    fn non_evm_receiver_row(&self, delivery: &NonEvmDelivery, cx: &App) -> gpui::Div {
        let receiver = delivery.receiver.clone();
        let short = railgun_ui::short_non_evm(&receiver);
        let label = self.non_evm_receiver_label(delivery.network, &receiver, cx);
        receiver_fact_row(receiver, short, label)
    }

    /// What a delivery under another attempt's lower approved minimum says of both minimums,
    /// as amounts on `delivery`'s network. `None` unless the trade and its delivery are on
    /// different attempts and the traded one approved more.
    pub(in crate::root::private_swap) fn earlier_minimum_note(
        &self,
        attempts: &SwapAttempts<'_>,
        delivery: BridgeDelivery,
        cx: &App,
    ) -> Option<String> {
        let (approved, unmet) = attempts.superseded_minimum()?;
        let amount = |amount| {
            self.network_token_amount(
                delivery.destination_chain,
                self.delivered_token(delivery, cx),
                amount,
                cx,
            )
        };
        Some(format!(
            "Delivered under an earlier order's approved minimum of {}. This order's minimum of {} did not apply.",
            amount(approved),
            amount(unmet)
        ))
    }
}
