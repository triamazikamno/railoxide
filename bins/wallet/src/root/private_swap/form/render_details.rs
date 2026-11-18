use super::*;

impl PrivateSwapsView {
    /// The rate and total costs; expanded, Railgun's and `CoW`'s fees, the gas estimate and the
    /// gas the user pays, the price tolerance, and the order's validity.
    pub(super) fn render_details(
        &self,
        form: &SwapForm,
        review: &Arc<SwapReview>,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let costs_label = self.costs_label(review, " in costs", cx);
        let open = form.details_open;
        let details = Self::details_header(
            open,
            app_text(self.rate_label(review, cx)),
            Some(costs_label),
            cx,
        );
        if !open {
            return details;
        }
        let shown = strip_review(form, review);
        let unshield_fee = plan.amount().saturating_sub(review.sell_amount());
        let percent = |bps: U256| format_bps_percent(u64::try_from(bps).unwrap_or(u64::MAX));
        let with_rate = |value: String, rate: String| {
            div()
                .flex()
                .items_baseline()
                .gap_1()
                .child(app_text(value))
                .child(app_muted_text(rate))
        };
        let content = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(detail_row(
                "Railgun unshield",
                with_rate(
                    self.token_amount(sell, unshield_fee, cx),
                    percent(review.unshield_fee_bps()),
                ),
                true,
                Some("Railgun's fee on the amount sold, taken before the order.".into()),
            ))
            .children(review.cow_fee().map(|fee| {
                detail_row(
                    "CoW fee",
                    app_text(self.token_amount(buy, fee, cx)),
                    true,
                    Some("CoW Protocol's fee, already out of the quote.".into()),
                )
            }))
            .child(detail_row(
                format!("Gas at {} gwei", format_gwei(review.gas_price_wei())),
                app_text(format!(
                    "≈ {}",
                    self.gas_money(buy, review.gas_estimate(), cx)
                )),
                true,
                Some("Gas for the trade and the private steps, at the current network gas price with a 25% cushion. Solvers may charge less.".into()),
            ))
            .child(detail_row(
                "Gas you pay",
                with_rate(
                    format!("up to {}", self.gas_money(buy, shown.gas_allowance(), cx)),
                    format_bps_percent(u64::from(shown.gas_share_bps())),
                ),
                true,
                None,
            ))
            .when(plan.delivery() == SwapDelivery::Reshield, |content| {
                content.child(detail_row(
                    "Railgun shield",
                    with_rate(
                        percent(review.shield_fee_bps()),
                        "of what you receive".to_owned(),
                    ),
                    true,
                    Some("Railgun's fee on shielding the bought tokens to your private balance.".into()),
                ))
            })
            .children(self.bridge_detail_rows(review, cx))
            // Below the anchor, the Buy card shows it instead.
            .children(
                price_delta(review)
                    .filter(|delta| review.bridge().is_none() && !delta.below())
                    .map(|delta| {
                        detail_row(
                            if delta.anchor == "Chainlink" {
                                "Price vs Chainlink"
                            } else {
                                "Price vs anchor"
                            },
                            delta.value(cx),
                            false,
                            delta.checked,
                        )
                    }),
            )
            .child(detail_row(
                "Price tolerance",
                Self::render_slippage(form, editable, cx),
                false,
                None,
            ))
            .child(Self::render_validity(form, review, with_setup, editable, cx));
        details.content(content)
    }

    /// The details section, collapsed: its `headline`, the total `costs`, and the chevron that
    /// opens it.
    pub(super) fn details_header(
        open: bool,
        headline: gpui::Div,
        costs: Option<String>,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let toggle = if open { "Hide details" } else { "Show details" };
        Collapsible::new()
            .open(open)
            .w_full()
            .min_w_0()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                app_button_base("swap-details-toggle")
                    .ghost()
                    .w_full()
                    .min_w_0()
                    .h_auto()
                    .min_h_8()
                    .px_0()
                    .py_1()
                    .accessibility_label(toggle)
                    .tooltip(toggle)
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(headline.flex_1().min_w_0().truncate())
                            .children(costs.map(|costs| app_muted_text(costs).flex_none()))
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .flex_none(),
                            ),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_details(cx))),
            )
    }

    /// Order valid for: 10, 30 or 60 minutes, in a popover. Choosing another quotes the swap
    /// again. Bridge delivery keeps the profile's window, which its bridge quote needs, so it
    /// shows that window and why.
    fn render_validity(
        form: &SwapForm,
        review: &SwapReview,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let bridge = form.bridged();
        let minutes = if bridge {
            review.valid_for()
        } else {
            form.valid_for
        }
        .as_secs()
            / 60;
        let label = if with_setup {
            format!("{minutes} minutes after setup")
        } else {
            format!("{minutes} minutes")
        };
        let value = if bridge {
            app_text(label).into_any_element()
        } else {
            let view = cx.entity();
            Popover::new("swap-validity")
                .anchor(Anchor::TopRight)
                .trigger(
                    app_button_base("swap-validity-trigger")
                        .ghost()
                        .xsmall()
                        .dropdown_caret(true)
                        .accessibility_label("Order valid for")
                        .child(app_button_label(label)),
                )
                .content(move |_, _, _| {
                    div()
                        .w(rems(16.))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(app_strong_text("Order valid for"))
                        .child(
                            ButtonGroup::new("swap-validity-choices")
                                .outline()
                                .compact()
                                .children(VALIDITY_MINUTES.into_iter().map(|choice| {
                                    let view = view.clone();
                                    app_segment_button(
                                        SharedString::from(format!("swap-validity-{choice}")),
                                        format!("{choice} min"),
                                        minutes == choice,
                                        !editable,
                                        None,
                                    )
                                    .on_click(move |_, window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.set_valid_for(
                                                Duration::from_mins(choice),
                                                window,
                                                cx,
                                            );
                                        });
                                    })
                                })),
                        )
                        .child(
                            app_muted_text(
                                "A longer window gives solvers more time to cover the gas, and keeps the published unshield runnable for longer.",
                            )
                            .whitespace_normal(),
                        )
                })
                .into_any_element()
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(detail_row("Order valid for", value, false, None))
            .when(bridge, |row| {
                row.child(
                    app_muted_text(format!(
                        "Bridge swaps keep the {minutes}-minute window their bridge quote needs."
                    ))
                    .debug_selector(|| "swap-validity-locked".into())
                    .text_xs()
                    .whitespace_normal(),
                )
            })
    }

    /// A Bridge quote's Bridge fee, in the bought token, and what the receiver gets on the
    /// destination network in place of Receive at least: Across's exact output, or NEAR
    /// Intents' minimum. A private delivery has its costs there instead: the delivery allowance
    /// and the shield fee's rate. Other quotes have none of them.
    fn bridge_detail_rows(&self, review: &SwapReview, cx: &App) -> Vec<gpui::Div> {
        // A non-EVM delivery's minimum there, in the record's token and decimals.
        if let (SwapDelivery::NonEvmBridge(delivery), Some(bridge)) =
            (review.plan().delivery(), review.bridge())
        {
            let buy = review.plan().buy_token();
            let mut rows = Vec::new();
            rows.extend(bridge.fee.map(|fee| {
                detail_row(
                    "Bridge fee",
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(app_text(self.with_usd(
                            self.token_amount(buy, fee, cx),
                            buy,
                            fee,
                            cx,
                        )))
                        .child(provider_mention(
                            BridgeProvider::NearIntents,
                            app_muted_text,
                        )),
                    true,
                    Some("NEAR Intents' fee, already out of the amount below.".into()),
                )
            }));
            rows.push(detail_row(
                format!("Receive on {}", delivery.network.display_name()),
                app_text(format!(
                    "at least {}",
                    non_evm_amount(&delivery, bridge.destination_minimum)
                )),
                false,
                Some("The minimum you approve in the review, after every fee.".into()),
            ));
            return rows;
        }
        let (SwapDelivery::Bridge(delivery), Some(bridge)) =
            (review.plan().delivery(), review.bridge())
        else {
            return Vec::new();
        };
        let buy = review.plan().buy_token();
        let provider = provider_name(delivery.provider);
        let receive = self.network_token_amount(
            delivery.destination_chain,
            self.bridge_received_token(delivery, cx),
            bridge.destination_minimum,
            cx,
        );
        let mut rows = Vec::new();
        rows.extend(bridge.fee.map(|fee| {
            detail_row(
                "Bridge fee",
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_text(self.with_usd(
                        self.token_amount(buy, fee, cx),
                        buy,
                        fee,
                        cx,
                    )))
                    .child(provider_mention(delivery.provider, app_muted_text)),
                true,
                Some(if bridge.private.is_some() {
                    format!("{provider}'s fee, already out of the minimum you receive.")
                } else {
                    format!("{provider}'s fee, already out of the amount below.")
                }),
            )
        }));
        let network = network_name(delivery.destination_chain);
        // A private delivery's costs on its network. The Buy card shows what is left of them.
        if let Some(private) = bridge.private {
            let percent = format_bps_percent(
                u64::try_from(private.destination_shield_fee_bps).unwrap_or(u64::MAX),
            );
            rows.push(detail_row(
                format!("Delivery on {network}"),
                app_text(format!(
                    "≈ {}",
                    self.network_token_amount(
                        delivery.destination_chain,
                        delivery.destination_token,
                        private.delivery_allowance,
                        cx
                    )
                )),
                true,
                Some(format!(
                    "What shielding on delivery costs the relayer in gas on {network}. An estimate, checked against Across's own quote when the order is placed."
                )),
            ));
            rows.push(detail_row(
                format!("Railgun shield on {network}"),
                div()
                    .flex()
                    .items_baseline()
                    .gap_1()
                    .child(app_text(percent))
                    .child(app_muted_text("of what you receive")),
                true,
                Some(format!(
                    "Railgun's fee on shielding the delivered tokens to your private balance on {network}."
                )),
            ));
            return rows;
        }
        rows.push(detail_row(
            format!("Receive on {network}"),
            match delivery.provider {
                BridgeProvider::Across => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_text(receive))
                    .child(app_muted_text("exactly")),
                BridgeProvider::NearIntents => app_text(format!("at least {receive}")),
            },
            false,
            Some("The minimum you approve in the review, after every fee.".into()),
        ));
        rows
    }

    /// The price tolerance presets in a popover. Choosing another preset quotes the swap again.
    /// The quote details, this popover among them, hide until a new quote is ready.
    pub(super) fn render_slippage(
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Popover {
        let view = cx.entity();
        let selected = form.slippage_bps;
        Popover::new("swap-slippage")
            .anchor(Anchor::TopRight)
            .trigger(
                app_button_base("swap-slippage-trigger")
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
                    .accessibility_label("Price tolerance")
                    .child(app_button_label(format_bps_percent(u64::from(selected)))),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(16.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_strong_text("Price tolerance"))
                    .child(
                        ButtonGroup::new("swap-slippage-presets")
                            .outline()
                            .compact()
                            .children(SLIPPAGE_CHOICES.into_iter().map(|(bps, label)| {
                                let view = view.clone();
                                app_segment_button(
                                    SharedString::from(format!("swap-slippage-{bps}")),
                                    label,
                                    selected == bps,
                                    !editable,
                                    None,
                                )
                                .on_click(move |_, window, cx| {
                                    view.update(cx, |view, cx| view.set_slippage(bps, window, cx));
                                })
                            })),
                    )
                    .child(
                        app_muted_text(
                            "A lower tolerance fills less often. An unfilled order costs only the setup.",
                        )
                        .whitespace_normal(),
                    )
            })
    }

    /// Show what keeps notes out of this swap, above the form.
    pub(super) fn open_locked_notes(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(session) = self.private_session() else {
            return;
        };
        crate::root::locked_notes::open_locked_notes_dialog(
            self.root.clone(),
            Arc::clone(session),
            self.runtime.clone(),
            window,
            cx,
        );
    }

    /// The amount-reduction prompt next to the amount it describes.
    pub(super) fn render_too_large(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let QuoteState::TooLarge { largest } = form.quote else {
            return None;
        };
        let entered = self.form_amount(form, cx).map_or_else(
            |_| "This amount".into(),
            |amount| self.token_amount(form.sell, amount, cx),
        );
        let largest_label = self.token_amount(form.sell, largest, cx);
        let message = format!(
            "{entered} is spread across more notes than one swap can spend. Up to {largest_label} fits."
        );
        if largest.is_zero() {
            return Some(error_alert("swap-too-large", message));
        }
        Some(action_alert(
            "swap-too-large",
            message,
            app_button("swap-use-largest", format!("Use {largest_label}"))
                .outline()
                .small()
                .flex_none()
                .disabled(!editable)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.use_amount(largest, window, cx);
                })),
            cx,
        ))
    }

    pub(super) fn render_setting_up_section(&self, form: &SwapForm) -> gpui::Div {
        let tracking = form
            .operation
            .and_then(|operation| self.tracking.get(&operation));
        let stage =
            tracking.and_then(|tracking| tracking.setup_progress(SetupSide::Origin)?.stage());
        let status = stage.map_or(
            "Waiting for the setup to be confirmed",
            TransactionGenerationStage::label,
        );
        // A setup's own problem stays with the stage it ended at, and the swap's error holds
        // what no stage shows.
        let setup_error = tracking.and_then(|tracking| {
            let mut problems = tracking
                .error
                .clone()
                .into_iter()
                .chain(
                    [SetupSide::Origin, SetupSide::Destination]
                        .into_iter()
                        .filter_map(|side| tracking.setup_progress(side)?.problem())
                        .map(|problem| String::from(&*problem)),
                )
                .collect::<Vec<_>>();
            problems.dedup();
            (!problems.is_empty()).then(|| problems.join(" "))
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(app_shimmer_text(format!(
                        "Setting up the swap's stealth {} · {status}",
                        if form.private_bridge() {
                            "accounts"
                        } else {
                            "account"
                        }
                    ))),
            )
            .children(setup_error.map(|error| error_alert("swap-setup-error", error)))
            .child(
                app_muted_text(
                    "Confirmation takes a few minutes. You can close this; the swap stays on the Private tab, and its order is placed once the setup is confirmed.",
                )
                .whitespace_normal(),
            )
    }
}
