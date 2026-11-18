use super::*;

impl PrivateSwapsView {
    /// The form of a swap paid from a Public account, and its footer. It is the swap form with
    /// what a Public source changes: Receive to is fixed, the flip is off, and there is one
    /// stealth account, on the destination.
    pub(in crate::root::private_swap::form) fn render_public_form(
        &self,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let Some(form) = self.form.as_ref() else {
            return (app_muted_text("The swap form closed."), None);
        };
        let Some(public) = form.public.as_ref() else {
            return (app_muted_text("Choose a Public account."), None);
        };
        let review = public.review.as_ref();
        let editable = !self.busy() && public.operation.is_none();
        let sell_assets = &form.assets.sell_assets;
        let high_cost = review.and_then(|review| self.public_high_cost(form, review, cx));
        let close_settings = cx.entity();
        let footer_close_settings = close_settings.clone();
        let body = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            // A press elsewhere in the form closes the setup broadcaster popover.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                close_settings.update(cx, Self::close_setup_settings);
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.render_sell_panel(form, sell_assets, editable, !editable, cx))
                    .child(
                        div()
                            .relative()
                            .child(self.render_buy_panel(form, editable, !editable, cx))
                            // The tokens are on different networks, so the flip is off.
                            .child(self.render_flip(form, sell_assets, false, false, cx)),
                    ),
            )
            .children(
                review
                    .filter(|review| !review.price_verified())
                    .map(|_| Self::price_acknowledgement(form, self.busy(), cx)),
            )
            .children(Self::render_cost_acknowledgement(
                form,
                high_cost.map(public_cost_warning),
                !self.busy(),
                cx,
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(labeled_row(
                        "Receive to",
                        app_text(form.network.map_or_else(
                            || "Private balance on another network".to_owned(),
                            |network| format!("Private balance on {}", network_name(network)),
                        )),
                    ))
                    .children(receive_to_problem(form)),
            )
            .children(
                form.network
                    .map(|network| self.render_shield_failure_row(form, network, editable, cx)),
            )
            .children(Self::render_blocked_shield_acknowledgement(
                form,
                BLOCKED_SHIELD_PUBLIC_ALTERNATIVES,
                !self.busy(),
                cx,
            ))
            .child(self.render_account_row(form, FormMode::Order, editable, cx))
            .children(review.map(|review| self.render_public_details(form, review, editable, cx)))
            // The reviewed swap is being claimed. Its detail shows what follows.
            .children(
                self.public_status
                    .clone()
                    .filter(|_| self.public_job.is_some())
                    .map(|status| {
                        div()
                            .debug_selector(|| "public-swap-status".into())
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Spinner::new().small())
                            .child(
                                app_shimmer_text(status)
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_normal(),
                            )
                    }),
            )
            .children(match &form.quote {
                QuoteState::Failed(error) => {
                    let failure = public_quote_failure(error, form.network, |token, amount| {
                        self.token_amount(token, amount, cx)
                    });
                    Some(alert_frame(
                        "public-swap-quote-error",
                        failure.message,
                        Self::quote_details(form, failure.details, cx),
                        None,
                        near_intents_status(error),
                        cx,
                    ))
                }
                _ => None,
            })
            .children(form.error.as_ref().map(|error| {
                // The error that names the account's CoW proxy has its address to copy.
                let proxy = public
                    .proxy_holds
                    .as_ref()
                    .filter(|(text, _)| text == error)
                    .map(|(_, proxy)| *proxy);
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .gap_1()
                    .child(
                        Alert::error("public-swap-form-error", error.clone())
                            .small()
                            .flex_1()
                            .min_w_0(),
                    )
                    .children(proxy.map(|proxy| {
                        clipboard_with_toast("public-swap-proxy-copy", proxy.to_checksum(None))
                            .xsmall()
                            .tooltip("Copy address")
                    }))
            }));
        // Why Review… is off, which its tooltip gives.
        let strip = public_strip(form);
        let blocker: Option<SharedString> = if let Some(reason) = self
            .public_form_reason(form, cx)
            .filter(|reason| reason.blocks_review)
        {
            Some(reason.text.into())
        } else if let Some(review) = review {
            if !review.price_verified() && !form.price_acknowledged {
                Some("Accept the unverified price before you review the swap.".into())
            } else if high_cost.is_some() && !form.high_costs_acknowledged {
                Some("Confirm Swap anyway to accept the high swap costs.".into())
            } else if form.blocked_shield_unaccepted() {
                Some(BLOCKED_SHIELD_PROBLEM.into())
            } else if let Some(chain_id) = self.destination_setup_chain(form).filter(|_| {
                // Debug UI fixture: its stand-in setup fee needs no estimate.
                form.destination_route.offered_estimate().is_none() && !ui_fixture::active()
            }) {
                Some(format!("Wait for the setup fee on {}.", network_name(chain_id)).into())
            } else if strip.is_some_and(|strip| strip.pending()) {
                Some(
                    form.bridge_quote_error
                        .clone()
                        .unwrap_or_else(|| "Updating the bridge quote…".into()),
                )
            } else if strip.is_some_and(|strip| strip.share_bps != form.gas_share_bps) {
                Some("Gas is too high for this swap right now.".into())
            } else {
                None
            }
        } else if matches!(form.quote, QuoteState::Loading) {
            Some("Wait for the quote.".into())
        } else {
            Some(NO_QUOTE_YET.into())
        };
        // Only an order is placed on CoW. A same-token deposit has no credit.
        let order = Self::public_form_path(form) != Some(PublicBridgePath::Deposit);
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                footer_close_settings.update(cx, Self::close_setup_settings);
            })
            .when(order, |footer| footer.child(powered_by_cow(cx)))
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button("public-swap-close", "Cancel")
                            .flex_none()
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.close_swap_dialog(window, cx);
                            })),
                    )
                    .child(
                        app_button("public-swap-review", "Review…")
                            .primary()
                            .flex_none()
                            .debug_selector(|| "public-swap-review".into())
                            .disabled(self.busy() || blocker.is_some())
                            .when_some(blocker.filter(|_| !self.busy()), Button::tooltip)
                            .loading(self.public_job.is_some())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.request_public_review(window, cx);
                            })),
                    ),
            );
        (body, Some(footer))
    }

    /// The decimals of the gas strip's amounts, the destination token's, for the Minimum
    /// field.
    pub(in crate::root::private_swap::form) fn public_strip_decimals(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Option<u8> {
        self.chain_token_metadata(form.network?, form.buy?, cx)
            .map(|metadata| metadata.decimals)
    }

    /// The gas strip of an order paid from a Public account: the swap form's row of share
    /// presets and its edit button, and under it, for a reviewed order, the bar and the
    /// Minimum field while they are open, a pending bridge refresh, and why a share the quote
    /// can't support shows no minimum. Its amounts are in the destination token.
    pub(in crate::root::private_swap::form) fn render_public_gas_strip(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let strip = public_strip(form);
        let order = form
            .public
            .as_ref()
            .and_then(|public| public.review.as_ref())
            .filter(|_| strip.is_some())
            .map(|review| (review, review.intent().bridged_token));
        // A quote that fell back from the form's share selects no preset.
        let fallback = strip.is_some_and(|strip| strip.share_bps != form.gas_share_bps);
        let selected = if form.gas_custom || fallback {
            None
        } else {
            GasPreset::of_share(strip.map_or(form.gas_share_bps, |strip| strip.share_bps))
        };
        let estimate_money = strip
            .zip(order)
            .map(|(strip, (_, bridged))| self.gas_money(bridged, strip.limit.gas_estimate, cx));
        let row = Self::render_gas_strip_row(
            form,
            selected,
            |preset| {
                let Some((review, bridged)) = order else {
                    return (false, None);
                };
                // A preset that would leave no positive minimum can't be chosen, and one whose
                // minimum solvers are unlikely to accept carries a warning.
                review.order_limit_at(preset.share_bps()).map_or_else(
                    |_| (true, None),
                    |limit| {
                        (
                            false,
                            self.fill_warning_for(
                                bridged,
                                limit.gas_estimate,
                                limit.best_case,
                                review.slippage_bps(),
                                limit.gas_allowance,
                                cx,
                            ),
                        )
                    },
                )
            },
            estimate_money.as_deref(),
            strip.is_some(),
            self.render_gas_help(form, None, cx),
            editable,
            cx,
        );
        let (Some(strip), Some((review, bridged)), Some(network), Some(buy)) =
            (strip, order, form.network, form.buy)
        else {
            return row;
        };
        let open = form.gas_bar_open();
        let bar = open.then(|| {
            // The bar starts at the minimum if the account's order pays all gas, or at zero
            // when the gas exceeds the swap, and ends at the best case.
            let (start, label) = review
                .order_limit_at(GAS_SHARE_LOOSE_BPS)
                .ok()
                .filter(|_| !strip.bar.gas_exceeds())
                .map_or((U256::ZERO, "gas exceeds the swap"), |loose| {
                    (loose.min_received, "fills most easily")
                });
            Self::render_gas_bar(
                form,
                strip.bar,
                strip.share_bps,
                (
                    self.network_bare_amount(network, buy, strip.scale.show(start), cx),
                    label,
                    self.network_bare_amount(
                        network,
                        buy,
                        review.best_case().unwrap_or_default(),
                        cx,
                    ),
                ),
                editable,
                cx,
            )
        });
        let minimum_field = open.then(|| {
            let usd = self
                .network_usd_micro_value(
                    network,
                    buy,
                    strip.scale.show(strip.limit.min_received),
                    cx,
                )
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
            Self::render_gas_minimum_field(
                form,
                self.network_token_symbol(network, buy, cx),
                usd,
                editable,
                cx,
            )
        });
        let refresh = strip
            .pending()
            .then(|| self.render_bridge_quote_error(form, editable, cx))
            .flatten();
        let warning = fallback.then(|| {
            div()
                .w_full()
                .min_w_0()
                .debug_selector(|| "swap-gas-too-high".into())
                .child(
                    Alert::warning(
                        "swap-gas-too-high",
                        self.gas_too_high_message(bridged, strip.limit.gas_estimate, cx),
                    )
                    .small()
                    .min_w_0(),
                )
        });
        row.children(bar)
            .children(minimum_field)
            .children(refresh)
            .children(warning)
    }

    /// The details section of a swap paid from a Public account: collapsed, an order's rate
    /// or that Across bridges the deposit, and the total costs; open, each cost and, for an
    /// order, the price check and the price tolerance.
    fn render_public_details(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let open = form.details_open;
        let headline = if review.intent().order {
            app_text(
                self.public_rate_label(form, review, cx)
                    .unwrap_or_else(|| "Unavailable".into()),
            )
        } else {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(app_text("Bridged by"))
                .child(provider_mention(BridgeProvider::Across, app_text))
        };
        let details = Self::details_header(
            open,
            headline,
            self.public_costs_total(form, review, cx)
                .map(|total| format!("{total} in costs")),
            cx,
        );
        if !open {
            return details;
        }
        details.content(
            div().w_full().min_w_0().flex().flex_col().gap_2().children(
                self.public_details(form, review, cx)
                    .into_iter()
                    .map(|row| {
                        let value = match row.value {
                            PublicDetailValue::Text { value, note } => div()
                                .flex()
                                .items_baseline()
                                .gap_1()
                                .child(app_text(value))
                                .children(note.map(app_muted_text))
                                .into_any_element(),
                            PublicDetailValue::Price(delta) => delta.value(cx).into_any_element(),
                            PublicDetailValue::Tolerance => {
                                Self::render_slippage(form, editable, cx).into_any_element()
                            }
                        };
                        detail_row(row.label, value, row.indented, row.help)
                    }),
            ),
        )
    }

    /// "1 DAI = 0.9996 USDC" at an order's quoted trading rate, excluding explicit fees.
    fn public_rate_label(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<String> {
        let sell = self.form_sell_metadata(form, cx)?;
        let bought = review.intent().bridged_token;
        let quote = review.quote()?;
        let rate = unit_rate(quote.sell_amount, sell.decimals, quote.buy_amount)?;
        Some(format!(
            "1 {} = {} {}",
            sell.symbol,
            railgun_ui::format_token_amount(rate, self.token_decimals(bought, cx)?),
            self.token_symbol(bought, cx)
        ))
    }

    /// "≈ $8.20": every cost of the details section in money. `None` while any of them has no
    /// cached rate, as they are in three tokens.
    pub(super) fn public_costs_total(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<String> {
        let (network, buy) = (form.network?, form.buy?);
        let bridged = review.intent().bridged_token;
        let bridge = review.bridge();
        let bounds = public_bounds(review);
        let on_origin = bounds
            .and_then(|bounds| bounds.gas_allowance)
            .unwrap_or_default()
            .saturating_add(bridge.fee.unwrap_or_default());
        let on_destination = bridge
            .private
            .map_or(U256::ZERO, |private| private.delivery_allowance)
            .saturating_add(
                bridge
                    .destination_minimum
                    .saturating_sub(bridge.received_minimum()),
            );
        let value = |chain: u64, token: Address, amount: U256| {
            if amount.is_zero() {
                Some(U256::ZERO)
            } else {
                self.network_usd_micro_value(chain, token, amount, cx)
            }
        };
        let total = value(self.origin_chain_id, bridged, on_origin)?
            .saturating_add(value(network, buy, on_destination)?)
            .saturating_add(value(
                self.origin_chain_id,
                Address::ZERO,
                review.gas_plan().max_gas_cost,
            )?);
        Some(format!("≈ {}", railgun_ui::format_usd_micro_value(total)))
    }

    /// The rows of the details section, in order: an order's gas, then what the bridge, the
    /// delivery and the shield take, the Public account's own gas, and for an order the price
    /// check and the price tolerance.
    pub(in crate::root::private_swap::form) fn public_details(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Vec<PublicDetail> {
        let Some((public, network, buy)) = form
            .public
            .as_ref()
            .zip(form.network)
            .zip(form.buy)
            .map(|((public, network), buy)| (public, network, buy))
        else {
            return Vec::new();
        };
        let order = review.intent().order;
        let bridged = review.intent().bridged_token;
        let bridge = review.bridge();
        let destination = network_name(network);
        let label = public_source_label(&public.account);
        let native = self.token_symbol(Address::ZERO, cx);
        let step = if order {
            "order is placed"
        } else {
            "deposit is sent"
        };
        let mut rows = Vec::new();
        if let Some(fee) = review.cow_fee() {
            rows.push(PublicDetail::cost(
                "CoW fee",
                self.token_amount(bridged, fee, cx),
                None,
                "CoW Protocol's fee, already out of the quote.".to_owned(),
            ));
        }
        if let Some(bounds) = public_bounds(review) {
            if let (Some(price), Some(estimate)) = (bounds.gas_price_wei, bounds.gas_estimate) {
                rows.push(PublicDetail::cost(
                    format!("Gas at {} gwei", format_gwei(price)),
                    format!("≈ {}", self.gas_money(bridged, estimate, cx)),
                    None,
                    "Gas for the trade and the bridge deposit, at the current network gas price with a 25% cushion. Solvers may charge less.".to_owned(),
                ));
            }
            // At the share the gas strip shows, as the Buy card's minimum is.
            if let Some(strip) = public_strip(form) {
                rows.push(PublicDetail {
                    label: "Gas you pay".to_owned(),
                    value: PublicDetailValue::Text {
                        value: format!(
                            "up to {}",
                            self.gas_money(bridged, strip.limit.gas_allowance, cx)
                        ),
                        note: Some(format_bps_percent(u64::from(strip.share_bps))),
                    },
                    indented: true,
                    help: None,
                });
            }
        }
        if let Some(fee) = bridge.fee {
            rows.push(PublicDetail::cost(
                "Across fee",
                self.token_amount(bridged, fee, cx),
                None,
                "Across's fee, already out of the amount you receive.".to_owned(),
            ));
        }
        if let Some(private) = bridge.private {
            let percent = format_bps_percent(
                u64::try_from(private.destination_shield_fee_bps).unwrap_or(u64::MAX),
            );
            rows.push(PublicDetail::cost(
                format!("Delivery on {destination}"),
                format!(
                    "≈ {}",
                    self.network_token_amount(network, buy, private.delivery_allowance, cx)
                ),
                None,
                format!(
                    "What shielding on delivery costs the relayer in gas on {destination}. An estimate, checked against Across's own quote when the {step}."
                ),
            ));
            // An order's amount is a minimum, so its shield fee is a rate. A deposit's is exact.
            let (value, note) = if order {
                (percent, "of what you receive".to_owned())
            } else {
                (
                    self.network_token_amount(
                        network,
                        buy,
                        bridge
                            .destination_minimum
                            .saturating_sub(bridge.received_minimum()),
                        cx,
                    ),
                    percent,
                )
            };
            rows.push(PublicDetail::cost(
                format!("Railgun shield on {destination}"),
                value,
                Some(note),
                format!(
                    "Railgun's fee on shielding the delivered tokens to your private balance on {destination}."
                ),
            ));
        }
        let max_gas = review.gas_plan().max_gas_cost;
        // An allowance that already covers the amount needs no approval, as the review says.
        let approves = !review.gas_plan().approval_gas_limits.is_empty();
        let permit = review.signs_permit();
        let up_to = format!("up to {}", self.token_amount(Address::ZERO, max_gas, cx));
        let usd = self.usd_label(Address::ZERO, max_gas, cx);
        rows.push(match (approves, order) {
            (true, true) => PublicDetail::cost(
                format!("Approval from {label}"),
                up_to,
                usd,
                format!(
                    "The most {label} pays in {native} for its approval. The swap and the bridge deposit are sent by a CoW solver."
                ),
            ),
            (false, true) if permit => PublicDetail::cost(
                format!("Approval from {label}"),
                "signed, not sent".to_owned(),
                Some("no gas".to_owned()),
                format!(
                    "{label} signs an approval that lets CoW take exactly the amount. It sends no transaction for it and pays no gas. The swap and the bridge deposit are sent by a CoW solver."
                ),
            ),
            (false, true) => PublicDetail::cost(
                format!("Approval from {label}"),
                "not needed".to_owned(),
                None,
                format!(
                    "{label} already lets CoW take the amount, so it sends nothing. The swap and the bridge deposit are sent by a CoW solver."
                ),
            ),
            (true, false) => PublicDetail::cost(
                format!("Approval and deposit from {label}"),
                up_to,
                usd,
                format!("The most {label} pays in {native} for its approval and the deposit."),
            ),
            (false, false) => PublicDetail::cost(
                format!("Deposit from {label}"),
                up_to,
                usd,
                format!("The most {label} pays in {native} for the deposit."),
            ),
        });
        if order {
            rows.extend(
                checked_price_delta(
                    review.price(),
                    review
                        .quote()
                        .map(|quote| (quote.sell_amount, quote.buy_amount)),
                )
                .map(|delta| PublicDetail {
                    label: if delta.anchor == "Chainlink" {
                        "Price vs Chainlink".to_owned()
                    } else {
                        "Price vs anchor".to_owned()
                    },
                    help: delta.checked.clone(),
                    value: PublicDetailValue::Price(delta),
                    indented: false,
                }),
            );
            rows.push(PublicDetail {
                label: "Price tolerance".to_owned(),
                value: PublicDetailValue::Tolerance,
                indented: false,
                help: None,
            });
        }
        rows
    }
}
