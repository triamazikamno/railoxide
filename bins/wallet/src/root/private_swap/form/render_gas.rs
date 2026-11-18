use super::*;

impl PrivateSwapsView {
    /// The gas strip across the bottom of the Buy panel: a row with the share presets and the
    /// edit button, and under it what a ready quote adds. Until a quote is ready, the row shows
    /// the form's share, and a preset chosen there prices the next quote.
    pub(super) fn render_gas_strip(
        &self,
        form: &SwapForm,
        review: Option<&Arc<SwapReview>>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let shown = review.map(|review| strip_review(form, review));
        let selected = form.selected_gas_preset(review);
        let estimate_money = shown
            .as_ref()
            .map(|shown| self.gas_money(shown.plan().buy_token(), shown.gas_estimate(), cx));
        let strip = Self::render_gas_strip_row(
            form,
            selected,
            |preset| {
                // A preset that would leave no positive minimum can't be chosen.
                let priced = review.map(|review| review.with_gas_share(preset.share_bps()));
                let unavailable = priced.as_ref().is_some_and(Result::is_err);
                // A preset whose minimum solvers are unlikely to accept carries a warning.
                let warning = priced
                    .and_then(Result::ok)
                    .and_then(|priced| self.fill_warning(&priced, cx));
                (unavailable, warning)
            },
            estimate_money.as_deref(),
            review.is_some(),
            self.render_gas_help(form, shown.as_deref(), cx),
            editable,
            cx,
        );
        let (Some(review), Some(shown)) = (review, &shown) else {
            return strip;
        };
        self.render_gas_quote(strip, form, review, shown, editable, cx)
    }

    /// The gas strip's row: the share presets, with `selected` chosen, and the edit button.
    /// `preset_state` says whether a preset leaves no positive minimum, and the warning on one
    /// solvers are unlikely to accept. `estimate_money` is the quote's gas estimate and `help`
    /// the info button. The edit button needs a ready quote, `quoted`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_gas_strip_row(
        form: &SwapForm,
        selected: Option<GasPreset>,
        preset_state: impl Fn(GasPreset) -> (bool, Option<String>),
        estimate_money: Option<&str>,
        quoted: bool,
        help: gpui::Stateful<gpui::Div>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        // The bar and the Minimum field show the quote's amounts.
        let open = quoted && form.gas_bar_open();
        let presets = ButtonGroup::new("swap-gas-presets")
            .outline()
            .compact()
            .disabled(!editable)
            .children(GasPreset::ALL.into_iter().map(|preset| {
                let (unavailable, warning) = preset_state(preset);
                let button = app_segment_button(
                    preset.id(),
                    preset.name(),
                    Some(preset) == selected,
                    !editable || unavailable,
                    warning.is_some().then(|| {
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(rgb(theme::WARNING))
                            .into_any_element()
                    }),
                )
                .debug_selector(|| preset.id().into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_gas_preset(preset, window, cx);
                }));
                match (estimate_money, warning) {
                    (Some(estimate_money), _) if unavailable => button.tooltip(format!(
                        "Gas (≈ {estimate_money}) is more than this swap returns"
                    )),
                    (_, Some(warning)) => button.tooltip(warning),
                    _ => button,
                }
            }));
        let edit = app_button_base("swap-gas-edit-minimum")
            .ghost()
            .xsmall()
            .icon(Icon::new(RailgunActionIcon::Pencil))
            .accessibility_label("Set a custom minimum")
            .tooltip("Set a custom minimum")
            .selected(open)
            .disabled(!editable || !quoted)
            .debug_selector(|| "swap-gas-edit-minimum".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.edit_gas_minimum(window, cx);
            }));
        div()
            .debug_selector(|| "swap-gas-strip".into())
            // Out to the panel's border, past its `px_3` and `py_2p5` padding.
            .mx(rems(-0.75))
            .mb(rems(-0.625))
            .mt_1()
            .px_3()
            .py_2p5()
            .rounded_b_lg()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .gap_y_1()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(app_muted_text("Minimum"))
                            .child(help),
                    )
                    // The presets keep their labels' width. A row too narrow for them wraps
                    // them under the label.
                    .child(
                        div()
                            .ml_auto()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(presets)
                            .child(edit),
                    ),
            )
    }

    /// What a ready quote adds under the gas strip's row: the bar and the Minimum field while
    /// they are open, a Bridge swap's pending refresh, and the warning on high authorized
    /// costs. Amounts are numbers only, as the Buy card names its token; a Bridge swap's are
    /// its destination amounts.
    fn render_gas_quote(
        &self,
        strip: gpui::Div,
        form: &SwapForm,
        review: &Arc<SwapReview>,
        shown: &SwapReview,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let open = form.gas_bar_open();
        let minimum_field = open.then(|| {
            Self::render_gas_minimum_field(
                form,
                self.strip_symbol(form, review, cx),
                self.strip_usd_label(form, review, shown.suggested_private_minimum(), cx),
                editable,
                cx,
            )
        });
        let quoting = matches!(
            self.form_mode(form),
            FormMode::Setup { .. } | FormMode::Order
        );
        // A share the bridge leg wasn't quoted at: a failed refresh says why and offers a full
        // quote.
        let refresh = form
            .gas_share_pending(review)
            .then(|| self.render_bridge_quote_error(form, editable, cx))
            .flatten();
        // A quote that fell back from the form's share says so instead of asking consent to
        // costs the user didn't choose.
        let warning = if form.gas_share_fallback(review) {
            quoting.then(|| {
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "swap-gas-too-high".into())
                    .child(
                        Alert::warning(
                            "swap-gas-too-high",
                            self.gas_too_high_message(
                                review.plan().buy_token(),
                                review.gas_estimate(),
                                cx,
                            ),
                        )
                        .small()
                        .min_w_0(),
                    )
            })
        } else {
            authorized_high_cost(review)
                .filter(|_| quoting)
                .and_then(|bps| {
                    Self::render_cost_acknowledgement(
                        form,
                        Some(self.authorized_cost_warning(review, bps, cx)),
                        editable,
                        cx,
                    )
                })
        };
        strip
            .children(open.then(|| {
                Self::render_gas_bar(
                    form,
                    GasBar::of(shown),
                    shown.gas_share_bps(),
                    self.gas_bar_ends(form, review, shown, cx),
                    editable,
                    cx,
                )
            }))
            .children(minimum_field)
            .children(refresh)
            .children(self.render_speed_up_minimums(form, review, shown, cx))
            .children(warning)
            .children(self.render_source_return(review, form.bridge_refreshing(review), cx))
    }

    /// Under a speed-up's Minimum controls: the open order's signed minimum beside the one the
    /// shown share signs, and, while that isn't lower, that it wouldn't speed the swap up.
    /// `None` for any other form, and for a quote that fell back from the form's share.
    fn render_speed_up_minimums(
        &self,
        form: &SwapForm,
        review: &Arc<SwapReview>,
        shown: &SwapReview,
        cx: &App,
    ) -> Option<gpui::Div> {
        let open = form.speed_up.as_ref()?;
        if form.gas_share_fallback(review) {
            return None;
        }
        let buy = shown.plan().buy_token();
        let signed = shown
            .buy_amount_for(shown.suggested_private_minimum())
            .ok()?;
        let lower = swap_speed_up_relaxes(&open.bounds, signed);
        // A Bridge swap's orders buy on the swap's own network, which the amounts name, as its
        // Buy panel shows the destination minimum.
        let amount = |amount: U256| {
            let amount = self.token_amount(buy, amount, cx);
            if form.bridged() {
                format!("{amount} on {}", self.chain_label())
            } else {
                amount
            }
        };
        Some(
            div()
                .debug_selector(|| "swap-speed-up-minimums".into())
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(detail_row(
                    "Open order's signed minimum",
                    app_text(amount(open.bounds.buy_amount)),
                    false,
                    None,
                ))
                .child(detail_row(
                    "New signed minimum",
                    app_text(amount(signed)),
                    false,
                    None,
                ))
                .when(!lower, |rows| {
                    rows.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .debug_selector(|| "swap-speed-up-not-lower".into())
                            .child(
                                Alert::warning("swap-speed-up-not-lower", SPEED_UP_NOT_LOWER)
                                    .small()
                                    .min_w_0(),
                            ),
                    )
                }),
        )
    }

    /// The Minimum field under the gas strip's row, in the token named by `symbol`, with the
    /// `usd` value of the shown minimum unless it repeats the field.
    pub(super) fn render_gas_minimum_field(
        form: &SwapForm,
        symbol: String,
        usd: Option<String>,
        editable: bool,
        cx: &App,
    ) -> gpui::Div {
        let usd =
            usd.filter(|usd| !usd_repeats_amount(usd, &form.gas_minimum_input.read(cx).value()));
        div()
            .debug_selector(|| "swap-gas-minimum-field".into())
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(app_muted_text("Minimum").flex_none())
            .child(
                div().w(rems(10.)).flex_none().child(
                    app_input(&form.gas_minimum_input)
                        .small()
                        .suffix(app_muted_text(symbol))
                        .disabled(!editable),
                ),
            )
            .children(usd.map(app_muted_text))
    }

    /// Under the gas strip while it shows a share the bridge leg wasn't quoted at and the
    /// refresh failed: why, with Retry.
    pub(super) fn render_bridge_quote_error(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let error = form.bridge_quote_error.as_ref()?;
        Some(action_alert(
            "swap-bridge-quote-error",
            error.to_string(),
            app_button("swap-bridge-quote-retry", "Retry")
                .debug_selector(|| "swap-bridge-quote-retry".into())
                .outline()
                .small()
                .flex_none()
                .disabled(!editable || self.busy())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.retry_quote(window, cx);
                })),
            cx,
        ))
    }

    /// The bar over the gas share, a `Slider` whose value is the knob's position in percent
    /// from "you pay all gas". A drawn track sits under the slider's transparent one: dotted
    /// from the start to the knob, the loss the minimum refuses, and solid from the knob to the
    /// end, where the swap can end up, with ticks at the other presets. Its focus takes the
    /// arrows in 5% steps, Home and End.
    pub(super) fn render_gas_bar(
        form: &SwapForm,
        bar: GasBar,
        share_bps: u16,
        (start, start_label, end): (String, &'static str, String),
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let position = f32::from(bar.position_bps(share_bps)) / 10_000.;
        let ticks = GasPreset::ALL
            .into_iter()
            .map(GasPreset::share_bps)
            .filter(|bps| *bps != share_bps && *bps <= bar.max_share_bps())
            .map(|bps| f32::from(bar.position_bps(bps)) / 10_000.)
            .collect::<Vec<_>>();
        let (solid, muted, ring) = (
            cx.theme().primary,
            cx.theme().muted_foreground,
            cx.theme().ring,
        );
        let track = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h_1p5()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(relative(position))
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w_full()
                                    .h_0()
                                    .border_t_2()
                                    .border_dashed()
                                    .border_color(muted),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(relative(position))
                            .right_0()
                            .rounded_full()
                            .bg(solid),
                    )
                    .children(ticks.into_iter().map(|tick| {
                        div()
                            .absolute()
                            .left(relative(tick))
                            .top(-rems(0.1875))
                            .w_0p5()
                            .h_3()
                            .bg(muted)
                    }))
                    // A disabled slider draws no thumb, so the knob is drawn here.
                    .when(!editable, |track| {
                        track.child(
                            div()
                                .absolute()
                                .left(relative(position))
                                .ml(-rems(0.5))
                                .top(-rems(0.3125))
                                .size_4()
                                .rounded_full()
                                .bg(solid),
                        )
                    }),
            );
        let end_label = |amount: String, label: &'static str| {
            div()
                .flex()
                .flex_col()
                .child(app_strong_text(amount).text_xs())
                .child(app_muted_text(label).text_xs())
        };
        div()
            .id("swap-gas-bar")
            .debug_selector(|| "swap-gas-bar".into())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(gpui::transparent_black())
            .when(editable, |element| {
                element
                    .track_focus(&form.gas_bar_focus)
                    .key_context(GAS_BAR_KEY_CONTEXT)
                    .focus_visible(move |style| style.border_color(ring))
                    .on_action(cx.listener(|this, _: &GasBarLeft, window, cx| {
                        this.step_gas_bar(|step| step.saturating_sub(GAS_BAR_STEP), window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarRight, window, cx| {
                        this.step_gas_bar(|step| step + GAS_BAR_STEP, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarStart, window, cx| {
                        this.step_gas_bar(|_| 0, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarEnd, window, cx| {
                        this.step_gas_bar(|_| 100, window, cx);
                    }))
            })
            .child(
                div().relative().w_full().h_6().child(track).child(
                    Slider::new(&form.gas_slider)
                        .reverse()
                        .bg(gpui::transparent_black())
                        .disabled(!editable),
                ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(end_label(start, start_label))
                    .child(end_label(end, "most you could get").items_end()),
            )
    }

    /// The gas bar's ends: its start and what it means, and the best case at its end. The start
    /// is the minimum if the user pays all gas, or zero when the gas exceeds the swap.
    pub(super) fn gas_bar_ends(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        shown: &SwapReview,
        cx: &App,
    ) -> (String, &'static str, String) {
        let scale = StripScale::of(review);
        let loose = shown
            .with_gas_share(GAS_SHARE_LOOSE_BPS)
            .ok()
            .filter(|_| !GasBar::of(shown).gas_exceeds());
        let (start, label) = loose.map_or((U256::ZERO, "gas exceeds the swap"), |loose| {
            (loose.suggested_private_minimum(), "fills most easily")
        });
        (
            self.strip_amount(form, review, scale.show(start), cx),
            label,
            self.strip_amount(form, review, scale.show(best_after_fees(shown)), cx),
        )
    }

    /// The strip's info button with its "Why pay less than the full gas?" explanation, the
    /// current quote's figures only: a tooltip on hover, and a popover on click or from the
    /// keyboard. The bar's legend is in it while the bar is open. Without a ready quote the
    /// explanation has no figures.
    pub(super) fn render_gas_help(
        &self,
        form: &SwapForm,
        shown: Option<&SwapReview>,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let trigger = app_button_base("swap-gas-help-trigger")
            .ghost()
            .xsmall()
            .icon(IconName::Info)
            .accessibility_label(GAS_HELP_TITLE)
            .debug_selector(|| "swap-gas-help-trigger".into());
        let view = cx.entity();
        let text = self.gas_help_text(shown, cx);
        let legend = form
            .gas_bar_open()
            .then(|| (cx.theme().primary, cx.theme().muted_foreground));
        let tooltip_text = text.clone();
        div()
            .id("swap-gas-help-hover")
            .flex_none()
            .when(!form.gas_help_open, |this| {
                this.tooltip(move |window, cx| {
                    let text = tooltip_text.clone();
                    Tooltip::element(move |window, _| gas_help_card(&text, legend, window))
                        .build(window, cx)
                })
            })
            .child(
                Popover::new("swap-gas-help")
                    .anchor(Anchor::TopLeft)
                    .open(form.gas_help_open)
                    .on_open_change(move |open, _, cx| {
                        view.update(cx, |view, cx| view.set_gas_help_open(*open, cx));
                    })
                    .trigger(trigger)
                    .content(move |_, window, _| {
                        gas_help_card(&text, legend, window)
                            .debug_selector(|| "swap-gas-help-content".into())
                    }),
            )
    }

    /// Why `review`'s minimum is unlikely to fill: the gas it allows, with the price tolerance a
    /// solver can also draw on, is below what settling tends to cost. `None` otherwise.
    fn fill_warning(&self, review: &SwapReview, cx: &App) -> Option<String> {
        self.fill_warning_for(
            review.plan().buy_token(),
            review.gas_estimate(),
            review.best_case(),
            review.slippage_bps(),
            review.gas_allowance(),
            cx,
        )
    }

    /// [`Self::fill_warning`] for an order that buys `buy`, from its gas estimate, best case
    /// and price tolerance and the gas its minimum allows.
    pub(super) fn fill_warning_for(
        &self,
        buy: Address,
        gas_estimate: U256,
        best_case: U256,
        slippage_bps: u32,
        allowed: U256,
        cx: &App,
    ) -> Option<String> {
        let bps = U256::from(10_000_u64);
        let realistic = gas_estimate * U256::from(REALISTIC_GAS_BPS) / bps;
        let tolerance = best_case * U256::from(slippage_bps) / bps;
        (allowed.saturating_add(tolerance) < realistic).then(|| {
            format!(
                "Unlikely to fill: settling this swap usually costs about {}, and this minimum leaves solvers {} for gas",
                self.gas_money(buy, realistic, cx),
                self.gas_money(buy, allowed, cx)
            )
        })
    }

    /// The info popover's paragraphs: the live gas estimate and what a lower minimum does;
    /// what adds gas; that solvers don't have to cover it; that the minimum is the only
    /// guarantee; and the fees that apply either way.
    fn gas_help_text(&self, review: Option<&SwapReview>, cx: &App) -> [String; 5] {
        let solvers = "Solvers compete to fill orders, and on small orders often pay all of the gas. They don't have to.".to_owned();
        let minimum = "Your minimum is the only amount the swap guarantees. Paying less of the gas raises it. If no solver accepts before the order expires, nothing is swapped and your funds stay private.".to_owned();
        // A Public account pays no Railgun fee on the swap's network, and keeps its tokens.
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            return [
                "Solvers usually cover some of this swap's network fees. The lower your minimum, the easier your order fills.".to_owned(),
                "The fees pay for the trade and for handing the tokens to the bridge after it.".to_owned(),
                solvers,
                "Your minimum is the only amount the swap guarantees. Paying less of the gas raises it. If no solver accepts before the order expires, nothing is swapped and your tokens stay in the Public account.".to_owned(),
                "CoW's fee and the bridge's costs apply either way.".to_owned(),
            ];
        }
        // Without a quote there are no figures to show.
        let Some(review) = review else {
            return [
                "A private swap costs more in network fees than a plain trade. Solvers usually cover some of it. The lower your minimum, the easier your order fills.".to_owned(),
                "The fees pay for the trade and for unshielding before it, and for shielding or the bridge hand-off after it.".to_owned(),
                solvers,
                minimum,
                "Railgun's fees and CoW's fee apply either way.".to_owned(),
            ];
        };
        let chain = self.chain_label();
        let steps = match review.plan().delivery() {
            SwapDelivery::Reshield => {
                format!("two steps on {chain}: unshielding before the trade and shielding after it")
            }
            SwapDelivery::External { .. } => {
                format!("a step on {chain}: unshielding before the trade")
            }
            SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_) => format!(
                "two steps on {chain}: unshielding before the trade and handing the tokens to the bridge after it"
            ),
        };
        let percent = |bps: U256| format_bps_percent(u64::try_from(bps).unwrap_or(u64::MAX));
        let (unshield, shield) = (
            percent(review.unshield_fee_bps()),
            percent(review.shield_fee_bps()),
        );
        let railgun = match review.plan().delivery() {
            SwapDelivery::Reshield if unshield == shield => {
                format!("Railgun's two {unshield} fees")
            }
            SwapDelivery::Reshield => {
                format!("Railgun's {unshield} unshield and {shield} shield fees")
            }
            _ => format!("Railgun's {unshield} unshield fee"),
        };
        [
            // An ordering, not a probability: a solver that fills a higher minimum fills a
            // lower one.
            format!(
                "This swap costs about {} in network fees. Solvers usually cover some of it. The lower your minimum, the easier your order fills.",
                self.gas_money(review.plan().buy_token(), review.gas_estimate(), cx)
            ),
            format!("The fees pay for the trade and for {steps}."),
            solvers,
            minimum,
            format!("{railgun} and CoW's fee apply either way."),
        ]
    }

    fn render_source_return(
        &self,
        review: &SwapReview,
        refreshing: bool,
        cx: &App,
    ) -> Option<gpui::Div> {
        let surplus = review.estimated_source_surplus()?;
        let token = review.plan().buy_token();
        let value = self.with_usd(
            format!("≈ {}", self.token_amount(token, surplus, cx)),
            token,
            surplus,
            cx,
        );
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .debug_selector(|| "swap-source-return".into())
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    app_muted_text(format!(
                                        "Estimated return on {}",
                                        self.chain_label()
                                    ))
                                    .min_w_0()
                                    .whitespace_normal(),
                                )
                                .child(source_return_help(review)),
                        )
                        .child(
                            if refreshing {
                                app_muted_shimmer_text(value)
                            } else {
                                app_text(value)
                            }
                            .flex_none(),
                        ),
                )
                .when_some(self.bridge_total_usd_value(review, cx), |row, total| {
                    let total = format!("≈ {}", railgun_ui::format_usd_micro_value(total));
                    row.child(
                        div()
                            .w_full()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .debug_selector(|| "swap-total-received".into())
                            .child(app_text("Estimated total received"))
                            .child(if refreshing {
                                app_muted_shimmer_text(total).font_weight(FontWeight::MEDIUM)
                            } else {
                                app_strong_text(total)
                            }),
                    )
                }),
        )
    }
}
