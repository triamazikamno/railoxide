use super::*;

impl PrivateSwapsView {
    /// The single review of a swap. With `setup`, the fees of a new swap's setup, which the
    /// review approves with its order, placed once the setup is confirmed; without, an order
    /// for a stealth account that is set up. With `approved`, the amounts that differ from
    /// those bounds show their difference. With `speed_up`, the open order the reviewed one
    /// speeds up, the review names both orders' signed minimums and that the open one stays
    /// live.
    pub(super) fn swap_summary(
        &self,
        review: &SwapReview,
        setup: Option<&[SetupFee]>,
        change: Option<SwapReviewChange>,
        approved: Option<&SwapApprovedBounds>,
        speed_up: Option<&SpeedUpOrder>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = review.valid_for().as_secs() / 60;
        let slippage = format_bps_percent(u64::from(review.slippage_bps()));
        let chain = self.chain_label();
        // The rows of the Costs line, then the rows after it.
        let (mut costs, mut rows) = (Vec::new(), Vec::new());
        if let Some(fees) = setup {
            costs.push(self.setup_fee_row(fees, cx));
        }
        // A Bridge order buys exactly this and deposits it with its provider.
        let minimum = self.token_amount(buy, review.suggested_private_minimum(), cx);
        let bridge = match plan.delivery() {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        let non_evm = match plan.delivery() {
            SwapDelivery::NonEvmBridge(delivery) => Some(delivery),
            _ => None,
        };
        let bridged = bridge.is_some() || non_evm.is_some();
        let private = bridge.filter(BridgeDelivery::is_private);
        let receive = match (bridge.zip(review.bridge()), &non_evm, review.bridge()) {
            // 1Click delivers at least the minimum, and about its expected output.
            (_, Some(delivery), Some(quote)) => self
                .non_evm_receive_card(
                    delivery,
                    Some(quote.expected_output),
                    quote.destination_minimum,
                    one_click_usd_estimate(quote, quote.destination_minimum),
                    cx,
                )
                .with_amount_change(
                    approved.and_then(|approved| approved.destination_minimum),
                    quote.destination_minimum,
                    false,
                    |amount| non_evm_amount(delivery, amount),
                ),
            (bridge_quote, _, _) => match bridge_quote {
                // The deposit fixes the destination minimum, less the shield fee there. Better
                // execution leaves surplus on the source chain, shown in the Returned row.
                Some((delivery, quote)) if delivery.is_private() => self
                    .bridge_receive_card(delivery, None, quote.received_minimum(), cx)
                    .with_amount_change(
                        approved.and_then(|approved| {
                            Some(private_delivery_credit(
                                approved.destination_minimum?,
                                approved,
                            ))
                        }),
                        quote.received_minimum(),
                        false,
                        |amount| {
                            self.network_token_amount(
                                delivery.destination_chain,
                                delivery.destination_token,
                                amount,
                                cx,
                            )
                        },
                    ),
                Some((delivery, quote)) => self
                    .bridge_receive_card(
                        delivery,
                        Some(quote.expected_output),
                        quote.destination_minimum,
                        cx,
                    )
                    .with_amount_change(
                        approved.and_then(|approved| approved.destination_minimum),
                        quote.destination_minimum,
                        false,
                        |amount| {
                            self.network_token_amount(
                                delivery.destination_chain,
                                self.bridge_received_token(delivery, cx),
                                amount,
                                cx,
                            )
                        },
                    ),
                None => self
                    .receive_card(review, review.suggested_private_minimum(), cx)
                    .with_amount_change(
                        approved.map(|approved| approved.private_minimum),
                        review.suggested_private_minimum(),
                        false,
                        |amount| self.token_amount(buy, amount, cx),
                    ),
            },
        };
        let estimate = self.gas_money(buy, review.gas_estimate(), cx);
        costs.push(
            self.gas_row(
                buy,
                review.gas_share_bps(),
                review.gas_allowance(),
                review.gas_estimate(),
                valid_for,
                cx,
            )
            .with_amount_change(
                approved.and_then(|approved| approved.gas_allowance),
                review.gas_allowance(),
                true,
                |amount| self.gas_money(buy, amount, cx),
            ),
        );
        let surplus = review.estimated_source_surplus();
        if let Some(surplus) = surplus {
            let hint = SpendAuthorizationHint::new(
                format!("Returned on {chain}"),
                [source_return_hint(review, &minimum, &chain)],
            );
            costs.push(
                SpendAuthorizationSummaryRow::new(
                    "Returned",
                    self.with_usd(
                        format!("≈ {}", self.token_amount(buy, surplus, cx)),
                        buy,
                        surplus,
                        cx,
                    ),
                )
                .with_hint(match self.bridge_total_usd_value(review, cx) {
                    Some(total) => hint.with_fact(
                        "Estimated total received",
                        format!("≈ {}", railgun_ui::format_usd_micro_value(total)),
                    ),
                    None => hint,
                }),
            );
        }
        if let Some(delivery) = bridge {
            costs.push(self.bridge_row(delivery, buy, Some(review), surplus.is_some(), cx));
        }
        if non_evm.is_some() {
            costs.push(self.non_evm_bridge_row(buy, Some(review), cx));
        }
        // What each order asks for on the swap's own network, where solvers fill it. A Bridge
        // swap's Receive card has the destination minimum, a term of its own.
        if let Some(open) = speed_up
            && let Ok(signed) = review.buy_amount_for(review.suggested_private_minimum())
        {
            let amount = |amount: U256| {
                let amount = self.token_amount(buy, amount, cx);
                if bridged {
                    format!("{amount} on {chain}")
                } else {
                    amount
                }
            };
            rows.push(SpendAuthorizationSummaryRow::new(
                "Open order's signed minimum",
                amount(open.bounds.buy_amount),
            ));
            rows.push(SpendAuthorizationSummaryRow::new(
                "New signed minimum",
                amount(signed),
            ));
        }
        if let Some(delivery) = private {
            rows.push(self.shield_failure_row(delivery, buy, cx));
        }
        // A non-EVM token's mint or contract tells look-alike symbols apart.
        rows.extend(non_evm.as_ref().and_then(non_evm_contract_row));
        // An earlier order's lower minimum, on the destination network like the Receive card's.
        let continuing = self.continuing_delivery(review, cx);
        if let Some(continuing) = &continuing {
            rows.push(
                SpendAuthorizationSummaryRow::new("Earlier approval", continuing.lower_there())
                    .with_hint(SpendAuthorizationHint::new(
                        "Earlier approval",
                        [EARLIER_APPROVAL_HINT],
                    )),
            );
        }
        // A private Bridge swap names both of its stealth accounts. Any other swap names its
        // own only when it reuses one.
        let accounts = self.review_accounts(plan.operation(), review);
        rows.extend(accounts.iter().map(ReviewAccount::row));
        if accounts.is_empty() && plan.swap_executor().is_reused() {
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Stealth account",
                    plan.executor().to_checksum(None),
                )
                .with_shortened_copyable(),
            );
        }
        let isolated = review.isolation() == OperationNetworkIsolation::Dedicated;
        if let OperationNetworkIsolation::Unavailable(mode) = review.isolation() {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Network route",
                format!("Not isolated in {mode} mode"),
            ));
        }
        let mut details = Vec::new();
        // A Bridge order signs its deposit, which the Deposit row shows, and a speed-up's
        // signed minimum is a row of its own.
        if !bridged
            && speed_up.is_none()
            && let Ok(signed) = review.buy_amount_for(review.suggested_private_minimum())
        {
            let signed = self.token_amount(buy, signed, cx);
            details.push((
                "Signed minimum",
                if plan.delivery() == SwapDelivery::Reshield {
                    format!("{signed}, before the shield fee")
                } else {
                    signed
                },
            ));
        }
        details.push((
            "Railgun unshield",
            self.token_amount(sell, plan.amount().saturating_sub(review.sell_amount()), cx),
        ));
        if let Some(fee) = review.cow_fee() {
            details.push(("CoW fee", self.token_amount(buy, fee, cx)));
        }
        details.push((
            "Gas estimate",
            format!(
                "≈ {estimate} at {} gwei",
                format_gwei(review.gas_price_wei())
            ),
        ));
        details.push(("Price tolerance", slippage.clone()));
        if let Some(provider) = bridge
            .map(|delivery| delivery.provider)
            .or_else(|| non_evm.as_ref().map(|_| BridgeProvider::NearIntents))
        {
            details.push((
                match provider {
                    BridgeProvider::Across => "Deposit to Across",
                    BridgeProvider::NearIntents => "Deposit to 1Click",
                },
                format!("{minimum} on {chain}"),
            ));
        }
        details.push((
            "Order valid for",
            if setup.is_some() {
                format!("{valid_for} minutes after setup")
            } else {
                format!("{valid_for} minutes")
            },
        ));
        // What becomes public: the line's summary, and the paragraphs of its card.
        let mut public = vec![
            "Placing the order publishes its tokens, amounts, price limit, and hook data, including the notes it spends, even if it never fills. A later spend of those notes can be linked to this swap.".to_owned(),
        ];
        let receiver = match plan.delivery() {
            SwapDelivery::Reshield => None,
            // A non-EVM receiver can't be one of the wallet's Public accounts.
            SwapDelivery::NonEvmBridge(delivery) => {
                public.push(near_intents_disclosure(delivery.network.display_name()));
                None
            }
            SwapDelivery::External { receiver } => {
                public.push(EXTERNAL_DELIVERY_DISCLOSURE.to_owned());
                Some((receiver, self.token_symbol(buy, cx)))
            }
            // The receiver is the swap's own stealth account on the destination network, so no
            // account of the user's is named.
            SwapDelivery::Bridge(delivery) if delivery.is_private() => {
                public = self.private_bridge_disclosure(delivery, &accounts);
                None
            }
            SwapDelivery::Bridge(delivery) => {
                public.push(self.bridge_disclosure(delivery));
                Some((
                    delivery.receiver,
                    self.network_token_symbol(
                        delivery.destination_chain,
                        self.bridge_received_token(delivery, cx),
                        cx,
                    ),
                ))
            }
        };
        if !isolated {
            public.push(
                "This network mode can't give the swap its own network route, so its orderbook requests aren't isolated from your other wallet traffic.".to_owned(),
            );
        }
        let own_account_warning = receiver
            .as_ref()
            .and_then(|(receiver, received)| self.own_account_warning(*receiver, received, cx));
        let public_summary = match (&receiver, &own_account_warning) {
            (None, _) if non_evm.is_some() => "the order, the receiver and the amount".to_owned(),
            (None, _) => private.map_or_else(
                || "the order and the notes it spends".to_owned(),
                |delivery| {
                    format!(
                        "this swap, and that it shields on {}",
                        network_name(delivery.destination_chain)
                    )
                },
            ),
            (Some((receiver, _)), Some(_)) => format!(
                "this swap, and {} as its receiver",
                spend_authorization_recipient_display(&receiver.to_checksum(None))
            ),
            (Some(_), None) => "the order, the receiver and the amount".to_owned(),
        };
        let public = SpendAuthorizationHint::new("What becomes public", public);
        let (title, confirm_label) = match setup {
            // The steps name the setup.
            Some([_]) => ("Private swap", "Create stealth account"),
            Some(_) => ("Private swap", "Create stealth accounts"),
            None if speed_up.is_some() => ("Speed up swap", "Speed up"),
            None => ("Private swap", "Swap"),
        };
        let note = if speed_up.is_some() {
            SPEED_UP_STAYS_LIVE
        } else {
            ""
        };
        let summary = SpendAuthorizationSummary::new(title, note, rows)
            .with_cards(
                self.sell_card(sell, plan.amount(), self.crosses_networks(bridge, bridged), cx)
                    .with_amount_change(
                        approved.map(SwapApprovedBounds::spend_amount),
                        plan.amount(),
                        true,
                        |amount| self.token_amount(sell, amount, cx),
                    ),
                receive,
            )
            .with_compact_rows()
            .with_row_group(
                "Costs",
                review_costs_summary(
                    Some(self.costs_label(review, "", cx)),
                    setup
                        .map(|fees| self.setup_fee_amounts(fees, cx))
                        .as_deref(),
                ),
                costs,
            )
            .with_details(
                "Order terms",
                format!("{slippage} price · {valid_for} min"),
                details,
                Some(
                    "If someone triggers the swap's unshield and the order doesn't fill, recovering the tokens costs the unshield and shield fees.",
                ),
            )
            .with_disclosure(
                public_summary,
                match own_account_warning {
                    Some(warning) => public.with_warning(warning),
                    None => public,
                },
            )
            .with_confirm_label(confirm_label);
        // The steps name as many setups as the review pays for: an existing account of a
        // private Bridge swap takes none.
        let summary = if let Some(fees) = setup {
            let two_accounts = fees.len() > 1;
            summary.with_steps(1, swap_steps(two_accounts), swap_steps_hint(two_accounts))
        } else {
            summary
        };
        let mut warnings = Vec::new();
        if private.is_some() {
            warnings.push(Arc::from(BLOCKED_SHIELD_REVIEW_WARNING));
        }
        if !review.price_verified() {
            warnings.push(Arc::from(unverified_price_warning(review)));
        }
        if let Some(bps) = authorized_high_cost(review) {
            warnings.push(Arc::from(
                self.authorized_cost_warning(review, bps, cx).message(),
            ));
        }
        if let Some(continuing) = &continuing {
            warnings.push(Arc::from(continuing.warning()));
        }
        if non_evm.is_some()
            || bridge.is_some_and(|delivery| delivery.provider == BridgeProvider::NearIntents)
        {
            warnings.push(Arc::from(NEAR_INTENTS_DISCLAIMER));
        }
        // Each reused account of a private Bridge swap has its own warning, which names it.
        warnings.extend(
            accounts
                .iter()
                .filter_map(ReviewAccount::reuse_warning)
                .map(Arc::from),
        );
        if accounts.is_empty() && plan.swap_executor().is_reused() {
            warnings.push(Arc::from(ACCOUNT_REUSE_NOTE));
        }
        if let Some(change) = change {
            warnings.push(Arc::from(format!(
                "Your last review wasn't used: {}. These are the new terms. Check them before you approve.",
                review_change_label(change)
            )));
        }
        summary.with_warnings(warnings)
    }

    /// A review's Gas row: the share's name, the most the user pays of the gas estimate, both
    /// in money of the `buy` token, and behind the info button what happens to the rest.
    pub(super) fn gas_row(
        &self,
        buy: Address,
        share_bps: u16,
        allowance: U256,
        estimate: U256,
        valid_for: u64,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let preset = gas_share_name(share_bps);
        let (allowed, estimate) = (
            self.gas_money(buy, allowance, cx),
            self.gas_money(buy, estimate, cx),
        );
        SpendAuthorizationSummaryRow::new(
            "Gas",
            format!("{preset} · up to {allowed} of ≈ {estimate}"),
        )
        .with_hint(SpendAuthorizationHint::new(
            "Gas you pay",
            [format!(
                "This swap costs about {estimate} in network fees. You pay at most {allowed} ({preset}). If no solver covers the rest within {valid_for} minutes, the order expires and nothing is swapped."
            )],
        ))
    }

    /// Both stealth accounts of the private Bridge swap `review` quotes, the swap's own
    /// first. `operation` is the swap's own account once it has one. Empty for any other
    /// delivery.
    ///
    /// An account is named by the address the swap's approval binds: the swap's own record's,
    /// and the delivery's receiver. A new account has neither until the swap reserves it. The
    /// swap's own account is a reused one when the quote was planned for one. The destination
    /// is the existing account the form chose, or the one the swap's use reserved, whose
    /// record tells whether the use set it up. While that record's network isn't loaded, the
    /// saved approval tells.
    pub(super) fn review_accounts(
        &self,
        operation: Option<ExecutorOperationId>,
        review: &SwapReview,
    ) -> Vec<ReviewAccount> {
        let plan = review.plan();
        let Some(delivery) = plan.delivery().private_bridge() else {
            return Vec::new();
        };
        let record = operation.and_then(|operation| self.record(operation));
        let source = ReviewAccount {
            role: "Source",
            chain_id: self.origin_chain_id,
            account: record.and_then(|record| Some((Some(record.index()), record.address()?))),
            reused: plan.swap_executor().is_reused(),
        };
        let chosen = self
            .form
            .as_ref()
            .and_then(SwapForm::destination_choice)
            .filter(|account| account.address == delivery.receiver);
        let reserved = record.and_then(|record| {
            let swap_use = record.swap_use(record.active_swap_use()?)?;
            let swap = crate::root::private_swap::model::SwapIdentity {
                operation: record.operation(),
                swap_use: swap_use.id(),
            };
            let destination = crate::root::private_swap::destination_account_metadata(
                self.swap_destination_account(swap, delivery),
                Some(swap.swap_use),
                swap_use.approval(),
            );
            let fresh = destination.fresh_from_record_or_approval();
            Some((destination.record.map(ExecutorRecord::index), !fresh))
        });
        let (account, reused) = match (chosen, delivery.receiver) {
            (Some(chosen), _) => (Some((Some(chosen.index), chosen.address)), true),
            // The stand-in receiver of an account the swap hasn't reserved.
            (None, Address::ZERO) => (None, false),
            (None, receiver) => {
                let (index, reused) = reserved.unwrap_or_default();
                (Some((index, receiver)), reused)
            }
        };
        vec![
            source,
            ReviewAccount {
                role: "Destination",
                chain_id: delivery.destination_chain,
                account,
                reused,
            },
        ]
    }

    /// The warning for External or Bridge delivery to a `receiver` that is one of the wallet's
    /// own Public accounts, which the swap links to a Railgun spend of the `received` token.
    pub(super) fn own_account_warning(
        &self,
        receiver: Address,
        received: &str,
        cx: &App,
    ) -> Option<String> {
        self.receiver_label(receiver, cx)
            .filter(|(_, own)| *own)
            .map(|(label, _)| {
                format!(
                    "{label} becomes publicly linked to this swap: anyone can see that it received {received} from a Railgun spend."
                )
            })
    }

    /// The review's Pay now row: each setup's fee limit in its own token, and behind the info
    /// button, who is paid from which private balance.
    pub(super) fn setup_fee_row(
        &self,
        fees: &[SetupFee],
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let amount = |fee: &SetupFee| self.setup_fee_amount(fee, cx);
        let total = self.setup_fee_amounts(fees, cx);
        let hint = match fees {
            [fee] => SpendAuthorizationHint::new(
                "Setup fee",
                [format!(
                    "Paid to broadcaster {} from your private balance to create the stealth account. Not refunded if the order doesn't fill.",
                    fee.broadcaster
                )],
            ),
            fees => SpendAuthorizationHint::new(
                "Setup fees",
                [
                    format!(
                        "Up to {}, each paid to a broadcaster from your private balance on that network.",
                        fees.iter()
                            .map(|fee| format!("{} on {}", amount(fee), network_name(fee.chain_id)))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                    "Neither is refunded if the order doesn't fill.".to_owned(),
                ],
            ),
        };
        SpendAuthorizationSummaryRow::new("Pay now", format!("up to {total} · not refunded"))
            .with_hint(hint)
    }

    /// Each setup's fee limit in its own token, as a Pay now row and the Costs line show them.
    fn setup_fee_amounts(&self, fees: &[SetupFee], cx: &App) -> String {
        fees.iter()
            .map(|fee| self.setup_fee_amount(fee, cx))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// A setup fee's limit in its own token, as a Pay now row shows it.
    pub(super) fn setup_fee_amount(&self, fee: &SetupFee, cx: &App) -> String {
        let root = self.root.upgrade();
        let registry = root
            .as_ref()
            .map(|root| &root.read(cx).effective_token_registry);
        format_token_amount_ceiling_for_display(fee.chain_id, fee.token, fee.maximum, registry)
    }

    /// A private Bridge swap's "If the shield fails" row: the choice its delivery carries, and
    /// behind the info button what that choice does with the `bought` token's deposit.
    pub(super) fn shield_failure_row(
        &self,
        delivery: BridgeDelivery,
        bought: Address,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let (origin, destination) = (self.chain_label(), network_name(delivery.destination_chain));
        let choice = delivery
            .private
            .map(|private| private.on_shield_failure)
            .unwrap_or_default();
        let label = shield_failure_label(choice, &origin, &destination);
        let paragraphs = match choice {
            BridgeShieldFailure::RefundOnOrigin => [
                format!(
                    "If the shield can't run, the delivery doesn't happen. Across returns the {} to the stealth account on {origin} after the deposit expires, usually within a few hours.",
                    self.token_symbol(bought, cx)
                ),
                "Recovering it costs a shield fee and a broadcaster fee.".to_owned(),
            ],
            BridgeShieldFailure::KeepOnDestination => [
                format!(
                    "If the shield can't run, the {} stays in the stealth account on {destination}, publicly visible, until you recover it there.",
                    self.network_token_symbol(
                        delivery.destination_chain,
                        delivery.destination_token,
                        cx
                    )
                ),
                format!(
                    "Relayers price this option less precisely, so the deposit is more likely to go unfilled and be refunded on {origin}."
                ),
            ],
        };
        SpendAuthorizationSummaryRow::naming_network(
            "If the shield fails",
            shield_failure_mention(choice, self.origin_chain_id, delivery.destination_chain),
        )
        .with_hint(SpendAuthorizationHint::new(label, paragraphs))
    }

    /// What a private Bridge swap makes public, for What becomes public: the deposit names the
    /// destination stealth account and its shield, which links the two networks, while the
    /// private address stays private. Keeping the tokens after a failed shield leaves them in
    /// view, and each reused account of `accounts` links the swap to its earlier activity.
    fn private_bridge_disclosure(
        &self,
        delivery: BridgeDelivery,
        accounts: &[ReviewAccount],
    ) -> Vec<String> {
        let (origin, destination) = (self.chain_label(), network_name(delivery.destination_chain));
        let mut public = vec![
            format!("The order, the notes it spends, and the Across deposit on {origin}."),
            format!(
                "The deposit names a stealth account on {destination} and the shield it runs. Anyone can see that this swap on {origin} became a shield of about this amount on {destination} at about this time."
            ),
            format!(
                "Your private address and what you do with the funds on {destination} afterwards stay private."
            ),
        ];
        if delivery.private.map(|private| private.on_shield_failure)
            == Some(BridgeShieldFailure::KeepOnDestination)
        {
            public.push(format!(
                "If the shield can't run, the tokens stay in the stealth account on {destination}, where they are publicly visible until you recover them."
            ));
        }
        public.extend(
            accounts
                .iter()
                .filter(|account| account.reused)
                .filter_map(|account| {
                    Some(format!(
                        "This swap reuses stealth account {}, so anyone can link it to that account's earlier public activity there.",
                        account.name()?
                    ))
                }),
        );
        public
    }

    /// Whether a review delivers on another network than the swap's: a `bridge` to another
    /// chain, or a non-EVM delivery, which is `bridged` without a `bridge`.
    pub(super) fn crosses_networks(&self, bridge: Option<BridgeDelivery>, bridged: bool) -> bool {
        bridge.map_or(bridged, |delivery| {
            delivery.destination_chain != self.origin_chain_id
        })
    }

    /// The review's Sell card. It names the swap's network when the swap delivers on another.
    pub(super) fn sell_card(
        &self,
        sell: Address,
        amount: U256,
        cross_network: bool,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let origin = self.origin_chain_id;
        let label = if cross_network {
            SpendAuthorizationLabel::on_network("Sell on", origin, "")
        } else {
            "Sell".into()
        };
        SpendAuthorizationCard::new(
            label,
            self.token_amount(sell, amount, cx),
            self.token_icon(sell, cx),
        )
        .with_network(origin)
        .with_usd(
            self.usd_label(sell, amount, cx)
                .filter(|usd| !usd_repeats_amount(usd, &self.bare_amount(sell, amount, cx))),
        )
    }

    /// `card` with the line naming `receiver`. Hovering it shows the wallet's name for the
    /// address, with one of its own Public accounts marked as such.
    fn with_receiver_line(
        &self,
        card: SpendAuthorizationCard,
        receiver: Address,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let label = self.receiver_label(receiver, cx).map(|(label, own)| {
            if own {
                format!("{label} · your Public account")
            } else {
                label
            }
        });
        card.with_receiver(receiver.to_checksum(None), label)
    }

    /// The Receive card of a swap that pays out on its own chain: at least `minimum`, the best
    /// case, and where it goes.
    pub(super) fn receive_card(
        &self,
        review: &SwapReview,
        minimum: U256,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let buy = review.plan().buy_token();
        let card = SpendAuthorizationCard::new(
            "Receive at least",
            self.token_amount(buy, minimum, cx),
            self.token_icon(buy, cx),
        )
        .with_network(self.origin_chain_id)
        .with_usd(
            self.usd_label(buy, minimum, cx)
                .filter(|usd| !usd_repeats_amount(usd, &self.bare_amount(buy, minimum, cx))),
        )
        .with_emphasis(
            "up to",
            self.bare_amount(buy, best_after_fees(review), cx),
            "if solvers pay all gas",
        );
        match review.plan().delivery() {
            SwapDelivery::Reshield => card.with_line("to your private balance"),
            SwapDelivery::External { receiver } => self.with_receiver_line(card, receiver, cx),
            SwapDelivery::Bridge(delivery) => self.with_receiver_line(card, delivery.receiver, cx),
            SwapDelivery::NonEvmBridge(delivery) => {
                let label = self.non_evm_receiver_label(delivery.network, &delivery.receiver, cx);
                let short = railgun_ui::short_non_evm(&delivery.receiver);
                card.with_short_receiver(delivery.receiver, short, label)
            }
        }
    }

    /// A non-EVM delivery's Receive card, in the record's token and decimals: at least
    /// `minimum`, about `expected` when it is known for this order, and its receiver with any
    /// saved label of that network. No anchor prices the token, so its `usd` value is only
    /// 1Click's estimate.
    pub(super) fn non_evm_receive_card(
        &self,
        delivery: &NonEvmDelivery,
        expected: Option<U256>,
        minimum: U256,
        usd: Option<String>,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let icon = non_evm_token_icon(
            delivery.network,
            delivery.contract.as_deref(),
            &delivery.symbol,
        );
        let card = SpendAuthorizationCard::new(
            format!("Receive on {}, at least", delivery.network.display_name()),
            non_evm_amount(delivery, minimum),
            icon,
        )
        .with_usd(usd);
        let card = match expected {
            Some(expected) => card.with_emphasis(
                "about",
                railgun_ui::format_token_amount(expected, delivery.decimals),
                "expected",
            ),
            None => card,
        };
        let label = self.non_evm_receiver_label(delivery.network, &delivery.receiver, cx);
        card.with_short_receiver(
            delivery.receiver.clone(),
            railgun_ui::short_non_evm(&delivery.receiver),
            label,
        )
    }

    /// A non-EVM delivery's Bridge row: NEAR Intents and, in the full `review`, its fee, as an
    /// EVM delivery's NEAR Intents row shows them.
    pub(super) fn non_evm_bridge_row(
        &self,
        bought: Address,
        review: Option<&SwapReview>,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let provider = BridgeProvider::NearIntents;
        let (value, paragraphs) = self.near_intents_row_terms(bought, review, cx);
        SpendAuthorizationSummaryRow::new("Bridge", value)
            .with_icon(Some(WalletIconSource::embedded(provider_mark_path(
                provider,
            ))))
            .with_hint(SpendAuthorizationHint::new(
                provider_name(provider),
                paragraphs,
            ))
    }

    /// A NEAR Intents Bridge row's value and the paragraphs of its hint, with the fee in the
    /// `bought` token when the full `review` values it.
    fn near_intents_row_terms(
        &self,
        bought: Address,
        review: Option<&SwapReview>,
        cx: &App,
    ) -> (String, Vec<String>) {
        let chain = self.chain_label();
        let provider = provider_name(BridgeProvider::NearIntents);
        let fee = review
            .and_then(SwapReview::bridge)
            .and_then(|bridge| bridge.fee);
        let via = format!(
            "Via {} on {chain}. The whole payout is converted, surplus included.",
            self.token_symbol(bought, cx)
        );
        match (review, fee) {
            (None, _) => (provider.to_owned(), vec![via]),
            // The fee's share of the deposit it was taken from.
            (Some(review), Some(fee)) => (
                format!("{provider} · fee ≈ {}", self.token_amount(bought, fee, cx)),
                vec![
                    via,
                    format!(
                        "The 1Click fee, about {}, is already in the minimum.",
                        format_bps_percent(swap_cost_bps(
                            fee,
                            review.suggested_private_minimum().saturating_sub(fee)
                        ))
                    ),
                ],
            ),
            // A leg between different assets without anchors has no value for its fee.
            (Some(_), None) => (
                format!("{provider} · fee included"),
                vec![
                    via,
                    "The 1Click fee is already in the minimum. There's no independent price to value it.".to_owned(),
                ],
            ),
        }
    }

    /// A Bridge swap's Receive card, in its network's token. Across delivers exactly the
    /// `minimum`; 1Click at least the `minimum`, and about `expected` when it is known for this
    /// order. A private delivery shows the guaranteed credit after the shield fee there;
    /// better execution does not increase the fixed Across deposit's destination amount.
    pub(super) fn bridge_receive_card(
        &self,
        delivery: BridgeDelivery,
        expected: Option<U256>,
        minimum: U256,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let (network, token) = (
            delivery.destination_chain,
            self.bridge_received_token(delivery, cx),
        );
        let card = |bound: &str| {
            self.destination_card(
                &format!(", {bound}"),
                network,
                token,
                delivery.destination_token,
                minimum,
                cx,
            )
        };
        let estimate = |card: SpendAuthorizationCard, before: &str, after: &str| match expected {
            Some(expected) => card.with_emphasis(
                before,
                self.network_bare_amount(network, token, expected, cx),
                after,
            ),
            None => card,
        };
        match (delivery.provider, delivery.is_private()) {
            // The receiver is the swap's own stealth account there, which shields what it gets.
            (BridgeProvider::Across, true) => card("at least").with_line("to your private balance"),
            (BridgeProvider::Across, false) => {
                self.with_receiver_line(card("exactly"), delivery.receiver, cx)
            }
            (BridgeProvider::NearIntents, _) => self.with_receiver_line(
                estimate(card("at least"), "about", "expected"),
                delivery.receiver,
                cx,
            ),
        }
    }

    /// A Receive card for `amount` of `token` on `network`, another one than the swap's, with
    /// its USD value at `priced`'s cached rate. Its label is "Receive on", the network, then
    /// `bound`, such as ", exactly".
    pub(super) fn destination_card(
        &self,
        bound: &str,
        network: u64,
        token: Address,
        priced: Address,
        amount: U256,
        cx: &App,
    ) -> SpendAuthorizationCard {
        SpendAuthorizationCard::new(
            SpendAuthorizationLabel::on_network("Receive on", network, bound),
            self.network_token_amount(network, token, amount, cx),
            self.chain_token_metadata(network, token, cx)
                .and_then(|metadata| metadata.icon_path),
        )
        .with_network(network)
        .with_usd(
            self.network_usd_micro_value(network, priced, amount, cx)
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
                .filter(|usd| {
                    !usd_repeats_amount(usd, &self.network_bare_amount(network, token, amount, cx))
                }),
        )
    }

    /// A Bridge swap's Bridge row: its provider and, in the full `review`, its fee, with the
    /// provider's terms behind the info button. The confirm-only step, without a `review`,
    /// shows no fee, since the minimum it leaves was approved. Without a Returned row, the
    /// hint says where Across leaves the surplus.
    pub(super) fn bridge_row(
        &self,
        delivery: BridgeDelivery,
        bought: Address,
        review: Option<&SwapReview>,
        returned: bool,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let chain = self.chain_label();
        let provider = provider_name(delivery.provider);
        let fee = review
            .and_then(SwapReview::bridge)
            .and_then(|bridge| bridge.fee);
        let (value, mut paragraphs) = match delivery.provider {
            BridgeProvider::Across => {
                let fee = fee.map(|fee| self.token_amount(bought, fee, cx));
                // A private delivery's allowance for the shield's gas on the destination
                // network, without its symbol for the row and with it for the card.
                let allowance = review
                    .and_then(SwapReview::bridge)
                    .and_then(|bridge| bridge.private)
                    .map(|private| {
                        let (network, token) =
                            (delivery.destination_chain, delivery.destination_token);
                        let allowance = private.delivery_allowance;
                        (
                            self.network_bare_amount(network, token, allowance, cx),
                            self.network_token_amount(network, token, allowance, cx),
                        )
                    });
                let mut paragraphs = if let (Some(fee), Some((_, allowance))) = (&fee, &allowance) {
                    vec![
                        across_delivery_note(fee, delivery.destination_chain, allowance),
                        "The estimate is checked against Across's own quote when the order is placed."
                            .to_owned(),
                    ]
                } else {
                    let mut paragraphs =
                        vec!["Relayers usually fill within a few minutes.".to_owned()];
                    if let Some(fee) = &fee {
                        paragraphs.push(format!(
                            "The {fee} fee covers relayer and LP fees and is already out of the amount you receive."
                        ));
                    }
                    paragraphs
                };
                if !returned {
                    paragraphs.push(match delivery.surplus {
                        BridgeSurplus::KeepInAccount => {
                            format!("CoW's surplus stays in the stealth account on {chain}.")
                        }
                        _ => format!("CoW's surplus is reshielded on {chain}."),
                    });
                }
                (
                    across_row_value(
                        provider,
                        fee.as_deref(),
                        allowance.as_ref().map(|(allowance, _)| allowance.as_str()),
                    ),
                    paragraphs,
                )
            }
            BridgeProvider::NearIntents => self.near_intents_row_terms(bought, review, cx),
        };
        // A Bridge order buys exactly the deposit, the quote's minimum.
        if let Some(review) = review
            && delivery.provider == BridgeProvider::Across
        {
            paragraphs.push(if delivery.is_private() {
                format!(
                    "If the deposit isn't filled before it expires, Across refunds it to the stealth account on {chain}, usually within a few hours."
                )
            } else {
                format!(
                    "If the deposit isn't filled before it expires, Across refunds the {} to the stealth account on {chain}, usually within a few hours. Recovering it to your private balance costs a shield fee and a broadcaster fee.",
                    self.token_amount(bought, review.suggested_private_minimum(), cx)
                )
            });
        }
        SpendAuthorizationSummaryRow::new("Bridge", value)
            .with_icon(Some(WalletIconSource::embedded(provider_mark_path(
                delivery.provider,
            ))))
            .with_hint(SpendAuthorizationHint::new(provider, paragraphs))
    }

    /// What a Bridge swap makes public across chains, for What becomes public.
    fn bridge_disclosure(&self, delivery: BridgeDelivery) -> String {
        let network = network_name(delivery.destination_chain);
        match delivery.provider {
            BridgeProvider::Across => format!(
                "The settlement on {} unshields from Railgun and makes the Across deposit in one transaction. The deposit names the receiver, {network} and the amounts, so the receiver's funds on {network} can be traced to this swap.",
                self.chain_label()
            ),
            BridgeProvider::NearIntents => near_intents_disclosure(&network),
        }
    }
}
