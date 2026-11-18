use super::*;

impl PrivateSwapsView {
    pub(in crate::root::private_swap::form) fn request_public_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_setup_route(cx);
        let waku = self.broadcaster_network(cx);
        let result = self.public_review_command(waku, cx);
        let command = match result {
            Ok(command) => Arc::new(command),
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        self.public_authorization = Some(command.clone());
        let view = cx.entity();
        let summary = command.summary.clone();
        let _ = self.root.update(cx, |root, cx| {
            root.request_spend_authorization(
                SpendAuthorizationIntent::PublicSwap(view, command),
                summary,
                window,
                cx,
            );
        });
        cx.notify();
    }

    fn public_review_command(
        &self,
        waku: Option<PublicBroadcasterWaku>,
        cx: &mut Context<'_, Self>,
    ) -> Result<PublicSwapAuthorization, String> {
        let form = self.form.as_ref().ok_or("The swap form closed.")?;
        let public = form.public.as_ref().ok_or("Choose a Public account.")?;
        let origin = self
            .public_origin(cx)
            .ok_or("The origin network is unavailable.")?;
        let network = form.network.ok_or("Choose a destination network.")?;
        let (destination_session, owner) = self.ready_destination(network, cx)?;
        let review = public.review.clone().ok_or("Wait for the quote.")?;
        let amount = self.form_amount(form, cx)?;
        // An open swap of the account that trades one of the draft's tokens, and the form's
        // other reasons, keep the review unavailable.
        if let Some(reason) = self
            .public_form_reason(form, cx)
            .filter(|reason| reason.blocks_review)
        {
            return Err(reason.text);
        }
        // The strip shows a share the bridge leg wasn't previewed at, or the quote can't
        // support the form's share.
        if let Some(strip) = public_strip(form) {
            if strip.pending() {
                return Err(form.bridge_quote_error.as_ref().map_or_else(
                    || "Updating the bridge quote…".to_owned(),
                    ToString::to_string,
                ));
            }
            if strip.share_bps != form.gas_share_bps {
                return Err("Gas is too high for this swap right now.".into());
            }
        }
        if amount != review.sell_amount()
            || !public
                .quote_terms
                .as_ref()
                .is_some_and(|terms| terms.matches(form, &origin, amount))
        {
            return Err(
                "The swap's source or terms changed. Quote it again before reviewing.".into(),
            );
        }
        if !review.price_verified() && !form.price_acknowledged {
            return Err(UNVERIFIED_PRICE_WARNING.into());
        }
        if self.public_high_cost(form, &review, cx).is_some() && !form.high_costs_acknowledged {
            return Err("A large share of this swap may go to costs. Review them and choose Swap anyway to continue.".into());
        }
        if form.blocked_shield_unaccepted() {
            return Err(BLOCKED_SHIELD_PROBLEM.into());
        }
        {
            let root = self.root.upgrade().ok_or("The wallet session ended.")?;
            let root = root.read(cx);
            if public.account.scope == PublicAccountScope::Global
                || !root
                    .public_accounts
                    .iter()
                    .any(|account| account == &public.account)
                || public.account.status != PublicAccountStatus::Active
                || matches!(
                    public.account.source,
                    PublicAccountSource::ExecutorDerived(_)
                )
            {
                return Err(
                    "Choose an active Public account scoped to this Private wallet.".into(),
                );
            }
        }
        if let Some(shortfall) = self.public_native_shortfall(form, &review, cx) {
            return Err(self.public_shortfall_text(form, shortfall, cx));
        }
        let held = public.operation.and_then(|operation| {
            self.public_records
                .iter()
                .find(|(_, record)| record.operation() == operation)
                .map(|(_, record)| record)
        });
        let held_swap =
            held.and_then(|record| public.swap_use.and_then(|id| record.public_swap_use(id)));
        // Debug UI fixture: a new account's stand-in setup, reviewed without a broadcaster.
        let fixture_setup = self
            .ui_fixture_setup(form, cx)
            .filter(|_| held_swap.is_none());
        let (account, approved, candidate, setup_fee) = if let Some((claimed, saved)) = held_swap {
            let fresh = claimed.is_fresh();
            let needs_setup = fresh && self.destination_setup_chain(form).is_some();
            let candidate = if needs_setup {
                // The broadcaster the fee was estimated for, which a repeated estimate
                // under way may differ from.
                Some(
                    form.destination_route
                        .offered_estimate()
                        .map(|estimate| estimate.broadcaster().clone())
                        .ok_or("Choose a setup broadcaster on the destination network.")?,
                )
            } else {
                None
            };
            let fee = match &candidate {
                Some(candidate) => Some(SwapSetupFeeLimit {
                    token: candidate.token,
                    amount: form
                        .destination_route
                        .offered_estimate()
                        .map(|estimate| default_public_broadcaster_fee_limit(estimate.fee_amount()))
                        .ok_or("Wait for the destination setup fee.")?,
                }),
                None => saved.approval().bounds.destination_setup_fee,
            };
            let operation = public.operation.expect("held operation");
            (
                if fresh {
                    SwapAccountChoice::New(operation)
                } else {
                    SwapAccountChoice::Existing(operation)
                },
                SwapApprovedAccount {
                    address: held.and_then(ExecutorRecord::address),
                    setup: fresh,
                },
                candidate,
                fee,
            )
        } else if let Some(selected) = form.destination_choice() {
            (
                SwapAccountChoice::Existing(selected.operation),
                SwapApprovedAccount {
                    address: Some(selected.address),
                    setup: false,
                },
                None,
                None,
            )
        } else {
            let route = &form.destination_route;
            let (candidate, fee) = if let Some(setup) = &fixture_setup {
                (
                    None,
                    SwapSetupFeeLimit {
                        token: setup.token,
                        amount: setup.maximum,
                    },
                )
            } else {
                // The broadcaster the fee was estimated for, which a repeated estimate
                // under way may differ from.
                let candidate = route
                    .offered_estimate()
                    .map(|estimate| estimate.broadcaster().clone())
                    .ok_or("Choose a setup broadcaster on the destination network.")?;
                let fee = route
                    .offered_estimate()
                    .map(|estimate| default_public_broadcaster_fee_limit(estimate.fee_amount()))
                    .ok_or("Wait for the destination setup fee.")?;
                let fee = SwapSetupFeeLimit {
                    token: candidate.token,
                    amount: fee,
                };
                (Some(candidate), fee)
            };
            let operation = public
                .operation
                .unwrap_or(ExecutorOperationId::random().map_err(|error| error.to_string())?);
            (
                SwapAccountChoice::New(operation),
                SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                candidate,
                Some(fee),
            )
        };
        let operation = account.operation();
        let swap_use = public
            .swap_use
            .unwrap_or(SwapUseId::random().map_err(|error| error.to_string())?);
        let approval = review
            .approval(approved, setup_fee, form.price_acknowledged)
            .map_err(|error| error.to_string())?;
        let clients = public
            .clients
            .clone()
            .ok_or("The swap's route is unavailable. Quote it again.")?;
        if candidate.is_some() && waku.is_none() {
            return Err("The broadcaster network isn't ready.".into());
        }
        // The setup this review pays for, when it sends one.
        let setup = candidate
            .as_ref()
            .zip(setup_fee)
            .map(|(candidate, limit)| SetupFee {
                chain_id: network,
                token: candidate.token,
                maximum: limit.amount,
                broadcaster: broadcaster_candidate_label(candidate),
            })
            .or(fixture_setup);
        let summary = self.public_review_summary(form, &review, &approval, setup.as_ref(), cx);
        let (anchors, registry) = {
            let root = self.root.upgrade().ok_or("The wallet closed.")?;
            let root = root.read(cx);
            (
                root.public_broadcaster_anchor_cache.clone(),
                root.effective_token_registry.clone(),
            )
        };
        Ok(PublicSwapAuthorization {
            source: public.account.clone(),
            origin,
            destination_session,
            owner,
            operation,
            swap_use,
            client: clients.http.clone(),
            action: PublicSwapCommand::Swap {
                account,
                destination_token: form.buy.ok_or("Choose a destination token.")?,
                candidate,
                review,
                quote_terms: public.quote_terms.clone().expect("matched review terms"),
                approval,
                clients,
                waku,
                anchors,
                registry,
            },
            summary,
            revision: form.quote_revision,
        })
    }

    pub(super) fn public_high_cost(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<u64> {
        let network = form.network?;
        let bridge = review.bridge();
        let sold = self.usd_micro_value(form.sell, review.sell_amount(), cx);
        let received =
            self.network_usd_micro_value(network, form.buy?, bridge.received_minimum(), cx);
        let gas = self.usd_micro_value(Address::ZERO, review.gas_plan().max_gas_cost, cx);
        let setup = if self.destination_setup_chain(form).is_some() {
            form.destination_route
                .estimate
                .as_ref()
                .and_then(|estimate| {
                    self.network_usd_micro_value(
                        network,
                        form.destination_route.fee_token?,
                        default_public_broadcaster_fee_limit(estimate.fee_amount()),
                        cx,
                    )
                })
        } else {
            Some(U256::ZERO)
        };
        let bps = if let (Some(sold), Some(received), Some(gas), Some(setup)) =
            (sold, received, gas, setup)
        {
            if sold.is_zero() {
                return None;
            }
            sold.saturating_sub(received)
                .saturating_add(gas)
                .saturating_add(setup)
                .saturating_mul(U256::from(10_000_u32))
                .checked_div(sold)?
                .saturating_to::<u64>()
        } else {
            // Without cached USD values, compare only amounts with the same denomination.
            let input = review.buy_amount().unwrap_or_else(|| review.sell_amount());
            let share = |cost: U256, amount: U256| {
                cost.saturating_mul(U256::from(10_000_u32))
                    .checked_div(amount)
                    .unwrap_or_default()
                    .saturating_to::<u64>()
            };
            let mut bps = share(bridge.fee.unwrap_or_default(), input);
            if let Some(private) = bridge.private {
                bps = bps
                    .saturating_add(share(private.delivery_allowance, private.quoted_output))
                    .saturating_add(private.destination_shield_fee_bps.saturating_to::<u64>());
            }
            if let Ok(approval) = review.approval(
                SwapApprovedAccount {
                    address: None,
                    setup: false,
                },
                None,
                true,
            ) {
                let allowance = approval.bounds.gas_allowance.unwrap_or_default();
                bps = bps.saturating_add(share(allowance, input.saturating_add(allowance)));
            }
            if form.sell == Address::ZERO {
                bps =
                    bps.saturating_add(share(review.gas_plan().max_gas_cost, review.sell_amount()));
            }
            bps
        };
        (bps >= AUTHORIZED_COST_WARNING_BPS).then_some(bps)
    }

    /// Whether a review that pays for no setup finds its fresh account already set up, as when
    /// a swap is continued. Its one dialog then is the last step, with nothing to wait for.
    fn public_setup_ready(
        &self,
        form: &SwapForm,
        approval: &PublicSwapApproval,
        pays_setup: bool,
    ) -> bool {
        approval.destination.setup
            && !pays_setup
            && matches!(
                self.public_destination_setup_progress(form),
                Some(SwapSetupProgress::Done)
            )
    }

    /// The compact review of a swap paid from a Public account. `setup` is the destination
    /// setup this review pays for.
    pub(in crate::root::private_swap::form) fn public_review_summary(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        approval: &PublicSwapApproval,
        setup: Option<&SetupFee>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let public = form.public.as_ref().expect("Public review");
        // The last step: the review of an account already set up.
        let ready = self.public_setup_ready(form, approval, setup.is_some());
        let destination = form.network.expect("Public destination");
        let (origin, network) = (self.chain_label(), network_name(destination));
        let label = public_source_label(&public.account);
        let order = review.intent().order;
        let bridged = review.intent().bridged_token;
        let buy = form.buy.unwrap_or_default();
        let bounds = &approval.bounds;
        let sold = self.form_sell_amount(form, review.sell_amount(), cx);
        let native = self.token_symbol(Address::ZERO, cx);
        let sell_metadata = self.form_sell_metadata(form, cx);
        let send_label = if order { "Sell" } else { "Send" };
        let send = SpendAuthorizationCard::new(
            if destination == self.origin_chain_id {
                SpendAuthorizationLabel::from(send_label)
            } else {
                SpendAuthorizationLabel::on_network(
                    format!("{send_label} on"),
                    self.origin_chain_id,
                    "",
                )
            },
            sold.clone(),
            sell_metadata
                .as_ref()
                .and_then(|metadata| metadata.icon_path.clone()),
        )
        .with_network(self.origin_chain_id)
        .with_usd(
            self.usd_label(form.sell, review.sell_amount(), cx)
                .filter(|usd| {
                    !usd_repeats_amount(
                        usd,
                        &sell_metadata.as_ref().map_or_else(String::new, |metadata| {
                            railgun_ui::format_token_amount(review.sell_amount(), metadata.decimals)
                        }),
                    )
                }),
        )
        .with_account(label.clone(), public.account.address.to_checksum(None));
        // An order's amount is its minimum. A deposit delivers what Across quoted.
        let receive = self.destination_card(
            if order { ", at least" } else { "" },
            destination,
            buy,
            buy,
            review.bridge().received_minimum(),
            cx,
        );
        // An order's best case, as the private review's Receive card shows it.
        let receive = match review.best_case() {
            Some(best) => receive.with_emphasis(
                "up to",
                self.network_bare_amount(destination, buy, best, cx),
                "if solvers pay all gas",
            ),
            None => receive,
        }
        .with_line("to your private balance");
        // The rows of the Costs line, then the rows after it.
        let (mut costs, mut rows) = (Vec::new(), Vec::new());
        let approvals = match review.gas_plan().approval_gas_limits.len() {
            0 => None,
            1 => Some("one approval"),
            _ => Some("two approvals"),
        };
        let sends = match (approvals, order) {
            (Some(approvals), true) => approvals.to_owned(),
            (None, true) => "no approval needed".to_owned(),
            (Some("one approval"), false) => "approval and deposit".to_owned(),
            (Some(approvals), false) => format!("{approvals} and deposit"),
            (None, false) => "deposit".to_owned(),
        };
        // A permit is the account's approval as a signature the order carries.
        let permit = review.signs_permit();
        // What the account does in the second step, which names an approval only when one is
        // sent or signed.
        let acts = match (approvals.is_some(), order) {
            (true, true) => "sends its approval and signs the order",
            (false, true) if permit => "signs its approval and the order",
            (false, true) => "signs the order",
            (true, false) => "sends its approval and the deposit",
            (false, false) => "sends the deposit",
        };
        let gas_paragraphs = match (approvals, order) {
            (Some(approvals), true) => vec![
                format!(
                    "{label} sends {approvals}, so CoW can take exactly {sold}. It pays that gas in {native}."
                ),
                "The swap and the bridge deposit are sent by a CoW solver. Their gas comes out of what you receive.".to_owned(),
            ],
            (None, true) if permit => vec![
                format!(
                    "{label} signs an approval that lets CoW take exactly {sold}. It sends no transaction for it and pays no gas."
                ),
                "The swap and the bridge deposit are sent by a CoW solver. Their gas comes out of what you receive.".to_owned(),
            ],
            (None, true) => vec![
                format!("{label} already lets CoW take {sold}, so it sends nothing."),
                "The swap and the bridge deposit are sent by a CoW solver. Their gas comes out of what you receive.".to_owned(),
            ],
            (Some(_), false) => vec![format!(
                "{label} sends the approval and the deposit itself, so Across takes exactly {sold}. It pays that gas in {native}."
            )],
            (None, false) => vec![format!(
                "{label} sends the deposit of {sold} itself. It pays that gas in {native}."
            )],
        };
        // What is paid up front outside the swap: a new account's setup fee and the gas of what
        // the Public account sends itself. An order whose approval stands sends nothing.
        let gas = (approvals.is_some() || !order)
            .then(|| self.token_amount(Address::ZERO, approval.max_gas_cost, cx));
        // A signed approval has a row of its own, as it is in no amount paid now.
        let signed_approval = permit.then(|| {
            SpendAuthorizationSummaryRow::new(
                "Approval",
                format!("signed by {label}, not sent · no gas"),
            )
            .with_hint(SpendAuthorizationHint::new(
                "Signed approval",
                gas_paragraphs.clone(),
            ))
        });
        // Neither the setup fee nor the account's own gas is refunded.
        let pay_now = if let Some(fee) = setup {
            let amount = self.setup_fee_amount(fee, cx);
            let (title, total) = match &gas {
                Some(gas) => ("Setup fee and gas", format!("{amount} + {gas}")),
                None => ("Setup fee", amount),
            };
            costs.push(
                SpendAuthorizationSummaryRow::new(
                    "Pay now",
                    format!("up to {total} · not refunded"),
                )
                .with_hint(SpendAuthorizationHint::new(
                    title,
                    std::iter::once(format!(
                        "Paid to broadcaster {} from your private balance on {} to create the stealth account there. Not refunded if the swap doesn't go through.",
                        fee.broadcaster,
                        network_name(fee.chain_id)
                    ))
                    .chain(gas_paragraphs),
                )),
            );
            Some(total)
        } else if let Some(gas) = gas {
            costs.push(
                SpendAuthorizationSummaryRow::new("Pay now", format!("up to {gas} · {sends}"))
                    .with_hint(SpendAuthorizationHint::new(
                        format!("Gas from {label}"),
                        gas_paragraphs,
                    )),
            );
            Some(gas)
        } else {
            None
        };
        costs.extend(signed_approval);
        let valid_for = u64::from(bounds.valid_for_secs.unwrap_or_default()) / 60;
        if let (Some(share), Some(allowance), Some(estimate)) = (
            bounds.gas_share_bps,
            bounds.gas_allowance,
            bounds.gas_estimate,
        ) {
            costs.push(self.gas_row(bridged, share, allowance, estimate, valid_for, cx));
        }
        let bridge = review.bridge();
        let fee = bridge.fee.map(|fee| self.token_amount(bridged, fee, cx));
        let allowance = bridge.private.map(|private| {
            (
                self.network_bare_amount(destination, buy, private.delivery_allowance, cx),
                self.network_token_amount(destination, buy, private.delivery_allowance, cx),
            )
        });
        costs.push(
            SpendAuthorizationSummaryRow::new(
                "Bridge",
                across_row_value(
                    provider_name(BridgeProvider::Across),
                    fee.as_deref(),
                    allowance.as_ref().map(|(allowance, _)| allowance.as_str()),
                ),
            )
            .with_icon(Some(WalletIconSource::embedded(provider_mark_path(
                BridgeProvider::Across,
            ))))
            .with_hint(SpendAuthorizationHint::new(
                provider_name(BridgeProvider::Across),
                fee.as_deref()
                    .zip(allowance.as_ref())
                    .map(|(fee, (_, allowance))| across_delivery_note(fee, destination, allowance))
                    .into_iter()
                    .chain([format!(
                        "If the deposit isn't filled before it expires, Across refunds it to {label} on {origin}, usually within a few hours."
                    )]),
            )),
        );
        // An existing account is named, with what reusing it links.
        let reused = (!approval.destination.setup)
            .then(|| self.public_reused_account(form))
            .flatten();
        let kind = if order { "swap" } else { "deposit" };
        let reuse_warning = reused.map(|(index, _)| {
            format!(
                "Reusing {} on {network} links this {kind} to that account's earlier swap.",
                index.map_or_else(
                    || "this account".to_owned(),
                    |index| format!("account #{index}")
                )
            )
        });
        if let Some((index, address)) = reused {
            rows.push(
                SpendAuthorizationSummaryRow::naming_network(
                    SpendAuthorizationLabel::on_network("Account on", destination, ""),
                    address.to_checksum(None),
                )
                .with_copyable_account(
                    index.map_or_else(
                        || "Reuses ".to_owned(),
                        |index| format!("Reuses #{index} · "),
                    ),
                    "stealth account address",
                )
                .with_hint(SpendAuthorizationHint::new(
                    format!("Account on {network}"),
                    reuse_warning
                        .clone()
                        .into_iter()
                        .chain(["A new stealth account offers more privacy.".to_owned()]),
                )),
            );
        }
        // The review states the choice made on the form, so its hint covers that choice only.
        let [refund, keep] = self.public_failure_paragraphs(form, destination, cx);
        let (failure, paragraph) = match approval.on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => refund,
            BridgeShieldFailure::KeepOnDestination => keep,
        };
        // A refund names the Public account, and a kept delivery its network.
        let failure_value = match approval.on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => SpendAuthorizationLabel::from(failure.clone()),
            choice @ BridgeShieldFailure::KeepOnDestination => {
                shield_failure_mention(choice, self.origin_chain_id, destination)
            }
        };
        rows.push(
            SpendAuthorizationSummaryRow::naming_network("If the shield fails", failure_value)
                .with_hint(SpendAuthorizationHint::new(
                    failure,
                    [
                        format!(
                            "This covers the rare case where the shield on {network} can't run."
                        ),
                        paragraph,
                    ],
                )),
        );
        let step = if order {
            "Approve and place order"
        } else {
            "Approve and deposit"
        };
        let title = match (ready, order, approval.destination.setup) {
            (true, ..) => step.to_owned(),
            (false, false, _) => format!("Shield on {network}"),
            (false, true, true) => "Set up account and swap".to_owned(),
            (false, true, false) => "Swap to private balance".to_owned(),
        };
        let mut public_paragraphs = vec![if order {
            format!("{label}'s order and its Across deposit on {origin}, with their amounts.")
        } else {
            format!("{label}'s Across deposit on {origin}, with its amount.")
        }];
        public_paragraphs.push(format!(
            "The deposit names a stealth account on {network} and the shield it runs. Anyone can see that {label} paid for a shield of about this amount on {network} at about this time."
        ));
        public_paragraphs.push(format!(
            "Your private address and what you do with the funds on {network} afterwards stay private."
        ));
        let mut summary = SpendAuthorizationSummary::new(title, "", rows)
            .with_cards(send, receive)
            .with_compact_rows()
            .with_row_group(
                "Costs",
                review_costs_summary(
                    self.public_costs_total(form, review, cx),
                    pay_now.as_deref(),
                ),
                costs,
            )
            .with_disclosure(
                format!("that {label} paid for a shield on {network}"),
                SpendAuthorizationHint::new("What becomes public", public_paragraphs),
            )
            .with_confirm_label(if approval.destination.setup && !ready {
                "Create stealth account"
            } else {
                step
            })
            .requiring_explicit_review();
        if order {
            let slippage = format_bps_percent(u64::from(bounds.slippage_bps));
            let mut details = Vec::new();
            if let (Some(estimate), Some(price)) = (bounds.gas_estimate, bounds.gas_price_wei) {
                details.push((
                    "Gas estimate",
                    format!(
                        "≈ {} at {} gwei",
                        self.gas_money(bridged, estimate, cx),
                        format_gwei(price)
                    ),
                ));
            }
            details.push(("Price tolerance", slippage.clone()));
            // The proxy the order goes through, with what it does behind the terms' note.
            let proxy = review.proxy().map(|proxy| {
                details.push(("Your CoW proxy", proxy.to_checksum(None)));
                format!(
                    "Your CoW proxy is a small contract that only {label} controls. The swap pays into it and, in the same transaction, it deposits everything into Across. If the deposit step fails, the {} stays in the proxy and you can withdraw it to {label}.",
                    self.token_symbol(bridged, cx)
                )
            });
            details.push((
                "Deposit to Across",
                format!(
                    "{} on {origin}",
                    self.token_amount(bridged, bounds.buy_amount, cx)
                ),
            ));
            details.push((
                "Order valid for",
                if approval.destination.setup && !ready {
                    format!("{valid_for} minutes after setup")
                } else {
                    format!("{valid_for} minutes")
                },
            ));
            summary = summary.with_details(
                "Order terms",
                format!("{slippage} price · {valid_for} min"),
                details,
                proxy.as_deref(),
            );
        }
        // A Public payer's swap always shields on another network.
        let mut warnings = vec![Arc::from(BLOCKED_SHIELD_REVIEW_WARNING)];
        if !review.price_verified() {
            warnings.push(Arc::from(UNVERIFIED_PRICE_WARNING));
        }
        if let Some(bps) = self.public_high_cost(form, review, cx) {
            warnings.push(Arc::from(public_cost_warning(bps).message()));
        }
        warnings.extend(reuse_warning.map(Arc::from));
        // As the private swap's next review says it.
        if let Some(change) = public.review_change {
            warnings.push(Arc::from(format!(
                "Your last review wasn't used: {}. These are the new terms. Check them before you approve.",
                review_change_label(change)
            )));
        }
        let summary = summary.with_warnings(warnings);
        // A same-token deposit to an existing account waits on nothing, so it has one step.
        match (approval.destination.setup, order) {
            (true, _) => summary.with_steps(
                if ready { 2 } else { 1 },
                [
                    SpendAuthorizationLabel::on_network("Set up account on", destination, ""),
                    step.into(),
                ],
                SpendAuthorizationHint::new(
                    "Two steps",
                    [
                        format!(
                            "1. Set up account on {network}. A broadcaster creates a one-time stealth account there, and you pay the setup fee from your private balance on {network}."
                        ),
                        format!(
                            "2. {step}. Once setup confirms, the wallet continues by itself and {label} {acts}. If the terms changed, you review them first."
                        ),
                    ],
                ),
            ),
            (false, true) => summary.with_steps(
                2,
                [
                    SpendAuthorizationLabel::on_network("Account on", destination, " ready"),
                    step.into(),
                ],
                SpendAuthorizationHint::new(
                    "Two steps",
                    [
                        format!(
                            "1. The stealth account on {network} is already set up, so there is no setup and no setup fee."
                        ),
                        format!("2. {step}. {label} {acts}."),
                    ],
                ),
            ),
            (false, false) => summary,
        }
    }

    /// The existing destination account the swap reuses: the one the form chose, or the one a
    /// continued swap holds. Its number is known once its record is read.
    fn public_reused_account(&self, form: &SwapForm) -> Option<(Option<u32>, Address)> {
        if let Some(account) = form.destination_choice() {
            return Some((Some(account.index), account.address));
        }
        let operation = form.public.as_ref()?.operation?;
        let (_, record) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)?;
        Some((Some(record.index()), record.address()?))
    }
}
