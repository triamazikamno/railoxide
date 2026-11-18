use super::*;

impl PrivateSwapsView {
    /// The form and its footer: the credit, Cancel, and the form's primary action.
    pub(in crate::root::private_swap) fn render_form(
        &self,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            return self.render_public_form(cx);
        }
        let Some(form) = self.form.as_ref() else {
            return (app_muted_text("Wallet session ended."), None);
        };
        let mode = self.form_mode(form);
        let quoting = matches!(mode, FormMode::Setup { .. } | FormMode::Order);
        let editable = quoting && !self.busy();
        let locked = form.operation.is_some() && !form.reuse_account;
        // A speed-up keeps its open order's terms. Only its Minimum controls take input.
        let terms_editable = editable && form.speed_up.is_none();
        let sell_assets = &form.assets.sell_assets;
        let review = match &form.quote {
            QuoteState::Ready(review) if quoting => Some(review),
            _ => None,
        };
        let close_settings = cx.entity();
        let footer_close_settings = close_settings.clone();
        let body = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            // A press elsewhere in the form closes the setup broadcaster popover. The popover
            // and the fee token list it opens draw above the form, so presses there don't.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                close_settings.update(cx, Self::close_setup_settings);
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.render_sell_panel(form, sell_assets, terms_editable, locked, cx))
                    .child(
                        div()
                            .relative()
                            .child(self.render_buy_panel(form, editable, locked, cx))
                            .child(self.render_flip(form, sell_assets, editable, locked, cx)),
                    ),
            )
            .children(self.render_price_acknowledgement(form, cx))
            .children(self.render_continuing_acknowledgement(form, editable, cx))
            .child(self.render_delivery(form, terms_editable, cx))
            .children(
                form.private_bridge()
                    .then(|| {
                        Self::render_blocked_shield_acknowledgement(
                            form,
                            BLOCKED_SHIELD_PRIVATE_ALTERNATIVES,
                            editable,
                            cx,
                        )
                    })
                    .flatten(),
            )
            .child(self.render_account_row(form, mode, terms_editable, cx))
            .children(review.map(|review| {
                self.render_details(
                    form,
                    review,
                    matches!(mode, FormMode::Setup { .. }),
                    terms_editable,
                    cx,
                )
            }))
            .children(form.error.as_ref().map(|error| {
                Alert::error("swap-form-error", error.clone())
                    .small()
                    .min_w_0()
            }));
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            // The footer sits outside the form, so it closes the popover the same way.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                footer_close_settings.update(cx, Self::close_setup_settings);
            })
            .child(powered_by_cow(cx))
            // The buttons wrap as one group, so a narrow dialog keeps them together on the
            // right.
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button("swap-form-cancel", "Cancel")
                            .flex_none()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_swap_dialog(window, cx);
                            })),
                    )
                    .child(self.render_form_primary(form, mode, cx)),
            );
        (body, Some(footer))
    }

    fn render_form_primary(
        &self,
        form: &SwapForm,
        mode: FormMode,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let busy = self.busy();
        match mode {
            FormMode::Setup { .. } | FormMode::SettingUp | FormMode::Order => {
                let blocker = self.review_blocker(form, mode, cx);
                app_button("swap-form-review", "Review…")
                    .primary()
                    .flex_none()
                    .loading(busy)
                    .disabled(busy || blocker.is_some())
                    .when_some(blocker.filter(|_| !busy), Button::tooltip)
                    .on_click(cx.listener(|this, _, window, cx| this.form_primary(window, cx)))
                    .into_any_element()
            }
            FormMode::Placed => {
                let operation = form.operation;
                app_button("swap-form-progress", "View progress…")
                    .flex_none()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(operation) = operation {
                            this.navigate(SwapDialogView::Detail(operation), window, cx);
                        }
                    }))
                    .into_any_element()
            }
        }
    }

    /// Why Review… is off in `mode`, which its tooltip gives, or `None` when the review can
    /// open. A review that also approves a setup needs that setup's fee: of each account the
    /// swap sets up, and of none for an existing account.
    fn review_blocker(&self, form: &SwapForm, mode: FormMode, cx: &App) -> Option<SharedString> {
        let QuoteState::Ready(review) = &form.quote else {
            return Some(form.delivery_problem().unwrap_or_else(|| {
                if matches!(form.quote, QuoteState::Loading) {
                    "Wait for the quote.".into()
                } else {
                    NO_QUOTE_YET.into()
                }
            }));
        };
        if let Some(problem) = form.review_problem(review) {
            return Some(problem);
        }
        if self.reviews_setup(form) {
            return [SetupSide::Origin, SetupSide::Destination]
                .into_iter()
                .find_map(|side| {
                    let chain_id = self.setup_chain(form, side)?;
                    let route = form.setup_route(side);
                    if route.offered_estimate().is_some() {
                        return None;
                    }
                    let network = network_name(chain_id);
                    let (status, problem) = self.setup_status(route, side, Some(chain_id), cx);
                    Some(if problem {
                        format!("Setup on {network}: {status}").into()
                    } else {
                        format!("Wait for the setup fee on {network}.").into()
                    })
                });
        }
        if mode != FormMode::Order {
            return Some("Wait for the stealth account's setup to be confirmed.".into());
        }
        self.destination_order_problem(form).map(Into::into)
    }

    /// The Sell panel: the amount, its token, what's available, and the amount-reduction
    /// prompt when the amount doesn't fit one swap.
    pub(super) fn render_sell_panel(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let asset = sell_assets.iter().find(|asset| asset.token == form.sell);
        let mut available = asset.map_or(U256::ZERO, |asset| match &form.quote {
            QuoteState::TooLarge { largest } => (*largest).min(asset.max_batched),
            _ => asset.max_batched,
        });
        if form.sell == Address::ZERO
            && let Some(review) = form
                .public
                .as_ref()
                .and_then(|public| public.review.as_ref())
        {
            available = available.saturating_sub(review.gas_plan().max_gas_cost);
        }
        let available_label = self.form_sell_amount(form, available, cx);
        let locked_value = asset.map_or(U256::ZERO, |_| form.assets.locked);
        let usd = self
            .form_amount(form, cx)
            .ok()
            .and_then(|amount| self.usd_label(form.sell, amount, cx))
            .filter(|usd| !usd_repeats_amount(usd, &form.amount_input.read(cx).value()));
        let too_large = self.render_too_large(form, editable, cx);
        // What keeps a swap paid from a Public account from being quoted or reviewed.
        let reason = self.public_form_reason(form, cx);
        let blocked = reason.as_ref().is_some_and(|reason| reason.blocks_review);
        let submit_view = cx.entity();
        let submit_enabled = editable
            && (matches!(form.quote, QuoteState::Ready(_))
                || form
                    .public
                    .as_ref()
                    .is_some_and(|public| public.review.is_some()))
            && !blocked;
        amount_panel(too_large.is_some() || blocked, cx)
            .child(Self::render_pay_from(form, editable && !locked))
            .children(self.render_pay_from_problem(cx))
            // Pay from names a Public-paid swap's source, so its amount needs no label.
            .when(form.public.is_none(), |panel| {
                panel.child(app_muted_text("Sell"))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_action(move |_: &InputEnter, window, cx| {
                                if submit_enabled {
                                    submit_view
                                        .update(cx, |view, cx| view.form_primary(window, cx));
                                }
                            })
                            .child(app_amount_input(&form.amount_input).disabled(!editable)),
                    )
                    .child(token_pill(ui::private_action::asset_select(
                        &form.sell_select,
                        locked || !editable,
                    ))),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .child(div().flex_1().min_w_0().children(usd.map(app_muted_text)))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .children(asset.map(|_| {
                                amount_max_button("swap-amount-max", Some(available_label))
                                    .debug_selector(|| "swap-amount-max".into())
                                    .disabled(!editable || available.is_zero())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.use_amount(available, window, cx);
                                    }))
                            }))
                            .when(!locked_value.is_zero(), |row| {
                                row.child(balance_text(format!(
                                    "{} locked",
                                    self.token_amount(form.sell, locked_value, cx)
                                )))
                                .child(
                                    app_button_base("swap-review-locked-notes")
                                        .label("Review")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                                        .tooltip("Show the locked notes and what holds them")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_locked_notes(window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .children(too_large)
            .children(reason.map(|reason| self.render_public_form_reason(reason, cx)))
    }

    /// Under Pay from on a network without a pinned `SpokePool`, where the Public accounts are
    /// listed but can't be chosen: why.
    fn render_pay_from_problem(&self, cx: &App) -> Option<gpui::Div> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        let bridges = root
            .effective_chain_configs
            .get(self.origin_chain_id)
            .and_then(EffectiveChainConfig::bridge_origin_profile)
            .is_some();
        let wallet = root.view_session.as_ref()?.wallet_id();
        let listed = root.public_accounts.iter().any(|account| {
            account.is_active_for_wallet(wallet)
                && !matches!(account.source, PublicAccountSource::ExecutorDerived(_))
        });
        // A note, not a problem: Private balance still pays from this network.
        (!bridges && listed).then(|| {
            div()
                .debug_selector(|| "swap-pay-from-problem".into())
                .w_full()
                .min_w_0()
                .child(
                    app_muted_text(format!(
                        "Public accounts can't bridge from {} yet.",
                        self.chain_label()
                    ))
                    .text_xs()
                    .whitespace_normal(),
                )
        })
    }

    /// The Buy panel: the delivery network in its label, the guaranteed minimum, its token's
    /// button, which opens the Buy picker, the price check and the best case, and the gas
    /// strip. The flip button sits on the seam above it.
    pub(super) fn render_buy_panel(
        &self,
        form: &SwapForm,
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        if let Some(public) = &form.public {
            let strip = public_strip(form);
            // A quote that fell back from the form's share has no minimum the user chose.
            let fallback = strip.is_some_and(|strip| strip.share_bps != form.gas_share_bps);
            // A minimum at a new share is priced locally until the bridge leg is previewed again.
            let refreshing =
                strip.is_some_and(|strip| strip.pending()) && form.bridge_quote_error.is_none();
            // An order's minimum at the share the strip shows, as it arrives on its network.
            // A deposit's amount is exact.
            let review = public.review.as_ref().filter(|_| !fallback);
            let minimum = review.map(|review| {
                strip.map_or_else(
                    || review.bridge().received_minimum(),
                    |strip| strip.scale.show(strip.limit.min_received),
                )
            });
            let pair = form.network.zip(form.buy);
            let amount = minimum.zip(pair).map(|(minimum, (network, buy))| {
                self.network_bare_amount(network, buy, minimum, cx)
            });
            // The minimum's value, then the best case, which solvers paying all gas leaves,
            // and a price below the anchor: the private form's line.
            let usd = minimum
                .zip(pair)
                .and_then(|(minimum, (network, buy))| {
                    self.network_usd_micro_value(network, buy, minimum, cx)
                })
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
                .filter(|usd| {
                    !amount
                        .as_ref()
                        .is_some_and(|amount| usd_repeats_amount(usd, amount))
                });
            let best = review
                .and_then(|review| review.best_case())
                .zip(pair)
                .map(|(best, (network, buy))| self.network_bare_amount(network, buy, best, cx));
            let delta = review
                .and_then(|review| {
                    checked_price_delta(
                        review.price(),
                        review
                            .quote()
                            .map(|quote| (quote.sell_amount, quote.buy_amount)),
                    )
                })
                .filter(PriceDelta::below);
            let up_to = (usd.is_some() || best.is_some()).then(|| {
                let separator = if best.is_some() { " ·" } else { "" };
                div()
                    .debug_selector(|| "swap-price-status".into())
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_x_1()
                    .children(usd.map(|usd| app_muted_text(format!("{usd}{separator}"))))
                    .children(best.map(|best| {
                        div()
                            .flex()
                            .items_baseline()
                            .gap_x_1()
                            .child(app_muted_text("up to"))
                            .child(app_strong_text(best))
                    }))
                    .children(delta.map(|delta| price_delta_note(delta, cx)))
            });
            // The same token on the other network is deposited as it is: its amount is exact
            // and it has no order, so no minimum to choose.
            let order = Self::public_form_path(form) != Some(PublicBridgePath::Deposit);
            let label = Self::public_buy_label(form, amount.is_some());
            let balance = form
                .network
                .zip(form.buy)
                .and_then(|(network, buy)| self.destination_balance_label(form, network, buy, cx));
            return amount_panel(false, cx)
                .debug_selector(|| "swap-buy-panel".into())
                .child(app_muted_text(label))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div().flex_1().min_w_0().child(match amount {
                                Some(amount) if refreshing => {
                                    app_amount_shimmer_text(amount).min_w_0().truncate()
                                }
                                Some(amount) => app_amount_text(amount).min_w_0().truncate(),
                                None => app_amount_text(if fallback { "—" } else { "0" })
                                    .text_color(rgb(theme::TEXT_SUBTLE)),
                            }),
                        )
                        .child(
                            token_pill(self.render_buy_token(form, editable && !locked, cx))
                                .debug_selector(|| "swap-buy-selector".into()),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().children(up_to.or_else(|| {
                            // A pending quote shows where the private form's does.
                            matches!(form.quote, QuoteState::Loading).then(|| {
                                div()
                                    .debug_selector(|| "swap-price-status".into())
                                    .min_w_0()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_x_2()
                                    .child(Spinner::new().small())
                                    .child(
                                        app_muted_shimmer_text(if order {
                                            "Getting a quote and checking the price…"
                                        } else {
                                            "Getting a quote…"
                                        })
                                        .flex_1()
                                        .min_w_0()
                                        .whitespace_normal(),
                                    )
                            })
                        })))
                        .children(balance.map(|balance| balance_text(balance).flex_none())),
                )
                .children(
                    (order && form.network.is_some())
                        .then(|| self.render_public_gas_strip(form, editable, cx)),
                );
        }
        let review = match &form.quote {
            QuoteState::Ready(review) => Some(review),
            _ => None,
        };
        // A quote that fell back from the form's share has no minimum the user chose.
        let fallback = review.is_some_and(|review| form.gas_share_fallback(review));
        // The minimum the order guarantees. A Bridge swap shows what arrives on its network, in
        // that network's token.
        let amount = review.filter(|_| !fallback).map(|review| {
            let minimum = strip_review(form, review).suggested_private_minimum();
            self.strip_amount(form, review, StripScale::of(review).show(minimum), cx)
        });
        let refreshing = review.is_some_and(|review| form.bridge_refreshing(review));
        // A quote's amount is a minimum, which the label says, so the amount starts at the
        // panel's edge like the Sell amount. The form has no other network control, so the
        // label names the network the Buy asset is delivered on.
        let network = form.network_key(self.origin_chain_id).name();
        let title = if amount.is_some() {
            format!("Buy on {network}, at least")
        } else {
            format!("Buy on {network}")
        };
        // The wallet's private balance of the Buy asset where it is delivered.
        let balance = form
            .buy
            .and_then(|buy| match (form.network, form.receive_to) {
                (None, _) => self.private_balance_label(form, buy, cx),
                (Some(network), ReceiveTo::PrivateBalance) => {
                    self.destination_balance_label(form, network, buy, cx)
                }
                (Some(_), ReceiveTo::PublicAddress) => None,
            });
        let same_token = matches!(form.bridge_state(), BridgeState::SameToken);
        // The strip's row is drawn before the quote is ready too, so the panel keeps its height.
        let strip = review.is_some()
            || matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        amount_panel(same_token, cx)
            .debug_selector(|| "swap-buy-panel".into())
            .child(app_muted_text(title))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(match amount {
                            Some(amount) if refreshing => {
                                app_amount_shimmer_text(amount).min_w_0().truncate()
                            }
                            Some(amount) => app_amount_text(amount).min_w_0().truncate(),
                            None => app_amount_text(if fallback { "—" } else { "0" })
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        }),
                    )
                    .children(self.render_bridge_notice(form, cx))
                    .child(
                        token_pill(self.render_buy_token(form, editable && !locked, cx))
                            .debug_selector(|| "swap-buy-selector".into()),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(
                        same_token
                            || matches!(
                                form.quote,
                                QuoteState::Failed(_) | QuoteState::PriceBlocked(_)
                            ),
                        gpui::Styled::items_start,
                    )
                    .child(self.render_price_line(form, cx).flex_1().min_w_0())
                    .children(balance.map(|balance| balance_text(balance).flex_none())),
            )
            .children(self.render_native_output(form, editable, locked, cx))
            .children(render_non_evm_contract(form))
            .children(strip.then(|| self.render_gas_strip(form, review, editable, cx)))
    }

    /// The Buy token button, with the delivery network's badge on the token's icon, and the
    /// Buy picker it opens. The picker takes the keyboard in its search, and gives it back when
    /// it closes. A swap whose tokens are fixed has the button alone.
    fn render_buy_token(
        &self,
        form: &SwapForm,
        enabled: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let badge = form.network_key(self.origin_chain_id).icon_path();
        let token = self.buy_token_display(form, cx);
        let label = token
            .as_ref()
            .map_or_else(|| "Select asset".to_owned(), |(symbol, _)| symbol.clone());
        app_button_base("swap-buy-token")
            .outline()
            .w_full()
            .disabled(!enabled)
            .accessibility_label(format!("Buy token: {label}"))
            .when(token.is_none() && enabled, |button| {
                button
                    .border_color(cx.theme().primary.opacity(0.65))
                    .bg(cx.theme().primary.opacity(0.12))
            })
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(token.map(|(_, icon)| badged_token_icon(icon, badge, 1.25)))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .on_click(cx.listener(|view, _, window, cx| view.open_buy_picker(window, cx)))
            .into_any_element()
    }

    /// Private Unshield's native/wrapped output switch, offered while a Public address receives
    /// the chain's wrapped native token.
    fn render_native_output(
        &self,
        form: &SwapForm,
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if !self.offers_native_output(form, cx) {
            return None;
        }
        let (native_label, wrapped_label) = native_wrapped_output_labels(self.origin_chain_id)?;
        let view = cx.entity();
        Some(
            ui::private_action::unshield_output_toggle(
                "swap-native-output",
                native_label,
                wrapped_label,
                form.native_output,
                locked || !editable,
                move |native, window, cx| {
                    view.update(cx, |view, cx| view.set_native_output(native, window, cx));
                },
            )
            .debug_selector(|| "swap-native-output".into()),
        )
    }

    pub(super) fn render_flip(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let reason = match form.buy {
            _ if locked => Some("This swap's tokens are fixed".to_owned()),
            _ if form.bridged() => Some("The tokens are on different networks".to_owned()),
            None => Some("Choose a token to receive first".to_owned()),
            Some(buy)
                if !sell_assets
                    .iter()
                    .any(|asset| asset.token == buy && !asset.max_batched.is_zero()) =>
            {
                Some(format!(
                    "No spendable private {} to sell",
                    self.token_symbol(buy, cx)
                ))
            }
            // The Buy list when `buy` is sold leaves `buy` out.
            Some(buy) if buy == form.sell || !form.assets.sell_receivable => Some(format!(
                "{} can't be received in a private swap",
                self.token_symbol(form.sell, cx)
            )),
            Some(_) => None,
        };
        let disabled = !editable || reason.is_some();
        let tooltip = reason.unwrap_or_else(|| "Switch the Sell and Buy tokens".to_owned());
        // Paint after the Buy panel as its sibling: GPUI paints a parent's border after its
        // children. Center the small button over the half-gap above the panel.
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(-rems(0.75 + 0.125))
            .flex()
            .justify_center()
            .child(
                app_button_base("swap-flip")
                    .small()
                    .border_1()
                    .border_color(cx.theme().border)
                    .icon(IconName::ArrowDown)
                    .accessibility_label("Switch tokens")
                    .tooltip(tooltip)
                    .disabled(disabled)
                    .debug_selector(|| "swap-flip".into())
                    .on_click(cx.listener(|this, _, window, cx| this.flip_tokens(window, cx))),
            )
    }

    /// The price check under the Buy amount. A failed or missing check replaces the anchor
    /// delta on the same line.
    fn render_price_line(&self, form: &SwapForm, cx: &Context<'_, Self>) -> gpui::Div {
        let retry = || {
            app_button("swap-price-retry", "Retry")
                .debug_selector(|| "swap-price-retry".into())
                .outline()
                .small()
                .flex_none()
                .disabled(self.busy() || form.quote_task.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.retry_quote(window, cx)))
        };
        let line = div()
            .debug_selector(|| "swap-price-status".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x_2();
        // A network still syncing or checking its setup fee shows its progress here, where
        // the quote will.
        if let Err(DeliveryProblem::Network {
            problem,
            syncing: true,
        }) = &form.delivery
        {
            return line
                .debug_selector(|| "swap-destination-progress".into())
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text(problem.clone())
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal(),
                );
        }
        // A network that can't take the delivery kind is explained under Receive to, and a
        // destination account that can't take the swap under its select.
        if matches!(
            form.delivery,
            Err(DeliveryProblem::Network { .. } | DeliveryProblem::Account(_))
        ) {
            return line;
        }
        // Until a provider delivers the Buy token, the line says why a Bridge swap can't be
        // quoted.
        let network = form.network_key(self.origin_chain_id).name();
        match form.bridge_state() {
            BridgeState::Loading => {
                return line.child(Spinner::new().small()).child(
                    app_muted_shimmer_text(format!("Getting the tokens {network} can receive…"))
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal(),
                );
            }
            BridgeState::Failed(error) => {
                let failure = self.quote_failure(error, cx);
                return line.child(alert_frame(
                    "swap-price-error",
                    failure.message,
                    Self::quote_details(form, failure.details, cx),
                    Some(self.bridge_routes_retry(cx)),
                    near_intents_status(error),
                    cx,
                ));
            }
            BridgeState::SameToken => {
                return line.child(error_alert("swap-bridge-same-token", SAME_TOKEN_BRIDGE));
            }
            BridgeState::Unavailable => {
                return line.child(error_alert(
                    "swap-bridge-unavailable",
                    format!(
                        "No bridge delivers this token to {network} now. Choose another token."
                    ),
                ));
            }
            BridgeState::Unreachable(provider) => {
                return line.child(action_alert(
                    "swap-bridge-unreachable",
                    format!(
                        "{} is unreachable, and no other bridge delivers this token to {network}.",
                        provider_name(provider)
                    ),
                    self.bridge_routes_retry(cx),
                    cx,
                ));
            }
            BridgeState::SameChain
            | BridgeState::NoToken
            | BridgeState::Ready { .. }
            | BridgeState::NonEvm(_) => {}
        }
        // The Provider row shows a provider's refusal of the amount.
        if let QuoteState::Failed(error) = &form.quote
            && bridge_rejection(error).is_some()
        {
            return line;
        }
        match &form.quote {
            QuoteState::Idle | QuoteState::TooLarge { .. } => line.child(
                app_muted_text(if form.has_buy() {
                    "Enter an amount that fits to get a quote"
                } else {
                    "Choose a token to receive"
                })
                // Shrinks beside the balance and wraps, instead of running under it.
                .flex_1()
                .min_w_0()
                .whitespace_normal(),
            ),
            QuoteState::Loading => line.child(Spinner::new().small()).child(
                app_muted_shimmer_text("Getting a quote and checking the price…")
                    .flex_1()
                    .min_w_0()
                    .whitespace_normal(),
            ),
            QuoteState::Failed(error) => {
                let failure = self.quote_failure(error, cx);
                line.child(alert_frame(
                    "swap-price-error",
                    failure.message,
                    Self::quote_details(form, failure.details, cx),
                    Some(retry()),
                    near_intents_status(error),
                    cx,
                ))
            }
            QuoteState::PriceBlocked(block) => {
                let text = match block {
                    PriceBlock::Deviates => {
                        let threshold = self
                            .swap_profile(cx)
                            .map_or(300, |profile| profile.anchor_deviation_bps());
                        format!(
                            "The exchange rate before fees is more than {} below the anchor price",
                            format_bps_percent(u64::from(threshold))
                        )
                    }
                };
                line.child(action_alert("swap-price-error", text, retry(), cx))
            }
            QuoteState::Ready(review) => {
                // A quote that fell back from the form's share shows no amounts, as the user
                // chose none of them. The gas strip says why.
                if form.gas_share_fallback(review) {
                    return line;
                }
                if let Some(quote) = review.bridge() {
                    let usd = if non_evm_bridge_review(review).is_some() {
                        one_click_usd_estimate(quote, quote.expected_output)
                    } else {
                        self.bridge_usd_value(review, cx)
                            .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
                    };
                    return line.debug_selector(|| "swap-destination-usd".into()).child(
                        app_muted_text(usd.unwrap_or_else(|| "USD value unavailable".to_owned())),
                    );
                }
                // The minimum's value, and the best case, which solvers paying all gas leaves.
                let buy = review.plan().buy_token();
                let shown = strip_review(form, review);
                let up_to = self.bare_amount(buy, best_after_fees(&shown), cx);
                let minimum = shown.suggested_private_minimum();
                let amount =
                    self.strip_amount(form, review, StripScale::of(review).show(minimum), cx);
                let usd = self
                    .usd_label(buy, minimum, cx)
                    .filter(|usd| !usd_repeats_amount(usd, &amount));
                let line = line.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .items_baseline()
                        .gap_x_1()
                        .children(usd.map(|usd| app_muted_text(format!("{usd} ·"))))
                        .child(app_muted_text("up to"))
                        .child(app_strong_text(up_to)),
                );
                match review.price() {
                    SwapPrice::Unverified => line,
                    // A quote at or above the anchor is in the details.
                    SwapPrice::Verified { .. } => line.children(
                        price_delta(review)
                            .filter(PriceDelta::below)
                            .map(|delta| price_delta_note(delta, cx)),
                    ),
                }
            }
        }
    }

    /// A Bridge swap's warning button, left of the Buy selector, while one provider couldn't be
    /// asked: a tooltip on hover, and a popover with Retry on click or from the keyboard. The
    /// price line says so instead while only that provider could deliver the Buy token.
    fn render_bridge_notice(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let provider = form.bridge_unavailable()?;
        // Private balance lists only what Across delivers, so nothing of NEAR Intents' is
        // missing.
        if form.receive_to == ReceiveTo::PrivateBalance
            || matches!(form.bridge_state(), BridgeState::Unreachable(_))
        {
            return None;
        }
        let provider = provider_name(provider);
        let view = cx.entity();
        let open_view = view.clone();
        let busy = self.busy();
        let open = form.bridge.notice_open;
        Some(
            div()
                .id("swap-bridge-partial")
                .debug_selector(|| "swap-bridge-partial".into())
                .flex_none()
                .when(!open, |this| {
                    this.tooltip(move |window, cx| {
                        Tooltip::element(move |window, _| bridge_notice_card(provider, window))
                            .build(window, cx)
                    })
                })
                .child(
                    Popover::new("swap-bridge-notice")
                        .anchor(Anchor::TopRight)
                        .open(open)
                        .on_open_change(move |open, _, cx| {
                            open_view.update(cx, |view, cx| {
                                view.set_bridge_notice_open(*open, cx);
                            });
                        })
                        .trigger(
                            app_button_base("swap-bridge-notice-trigger")
                                .ghost()
                                .xsmall()
                                .icon(
                                    Icon::new(IconName::TriangleAlert)
                                        .text_color(rgb(theme::WARNING)),
                                )
                                .accessibility_label(format!("{provider} is unreachable"))
                                .debug_selector(|| "swap-bridge-notice-trigger".into()),
                        )
                        .content(move |_, window, _| {
                            bridge_notice_card(provider, window).child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .child(bridge_routes_retry_button(view.clone(), busy)),
                            )
                        }),
                ),
        )
    }

    /// Retry for a Bridge swap's routes.
    pub(super) fn bridge_routes_retry(&self, cx: &Context<'_, Self>) -> Button {
        bridge_routes_retry_button(cx.entity(), self.busy())
    }

    /// A failed quote's message and details, with its estimated cost as a token amount.
    pub(super) fn quote_failure(&self, error: &eyre::Report, cx: &App) -> QuoteFailure {
        map_quote_failure(error, |token, amount| self.token_amount(token, amount, cx))
    }

    pub(super) fn quote_error_message(&self, error: &eyre::Report, cx: &App) -> String {
        self.quote_failure(error, cx).message
    }

    /// The Details section of a failed quote's alert, for a failure that has `details`.
    pub(super) fn quote_details(
        form: &SwapForm,
        details: Option<String>,
        cx: &Context<'_, Self>,
    ) -> Option<AlertDetails> {
        details.map(|text| AlertDetails {
            text,
            open: form.quote_details_open,
            view: cx.entity(),
        })
    }
}
