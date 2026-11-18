use super::*;

impl PrivateSwapsView {
    pub(super) fn render_price_acknowledgement(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let QuoteState::Ready(review) = &form.quote else {
            return None;
        };
        (!review.price_verified()).then(|| {
            // A share still being quoted shows estimated terms, which can't be accepted.
            Self::price_acknowledgement(form, self.busy() || form.gas_share_pending(review), cx)
        })
    }

    /// The unverified-price warning with its consent checkbox inside the warning frame.
    pub(super) fn price_acknowledgement(
        form: &SwapForm,
        disabled: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let warning_color = cx.theme().warning;
        let warning = match &form.quote {
            QuoteState::Ready(review) => unverified_price_warning(review),
            _ => UNVERIFIED_PRICE_WARNING.to_owned(),
        };
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(warning_color.mix_oklab(gpui::transparent_white(), 0.3))
            .bg(warning_color.mix_oklab(gpui::transparent_white(), 0.04))
            .child(
                Alert::warning("swap-unverified-price", warning)
                    .small()
                    .min_w_0()
                    .font_weight(FontWeight::SEMIBOLD)
                    .p_0()
                    .border_0()
                    .bg(gpui::transparent_black()),
            )
            .child(
                Checkbox::new("swap-price-acknowledged")
                    .label("I accept this price without an independent check")
                    .checked(form.price_acknowledged)
                    .small()
                    .disabled(disabled)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(form) = this.form.as_mut() {
                            form.price_acknowledged = *checked;
                            form.error = None;
                        }
                        cx.notify();
                    })),
            )
    }

    /// The warning that an earlier order's approved deposit can deliver less than this quote,
    /// with its consent checkbox inside the warning frame.
    pub(super) fn render_continuing_acknowledgement(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let QuoteState::Ready(review) = &form.quote else {
            return None;
        };
        let continuing = self.continuing_delivery(review, cx)?;
        let warning_color = cx.theme().warning;
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(warning_color.mix_oklab(gpui::transparent_white(), 0.3))
                .bg(warning_color.mix_oklab(gpui::transparent_white(), 0.04))
                .debug_selector(|| "swap-continuing-approval".into())
                .child(
                    Alert::warning("swap-continuing-approval", continuing.headline())
                        .small()
                        .min_w_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .p_0()
                        .border_0()
                        .bg(gpui::transparent_black()),
                )
                .child(
                    app_muted_text(continuing.details(&self.chain_label()))
                        .min_w_0()
                        .whitespace_normal(),
                )
                .child(
                    div()
                        .debug_selector(|| "swap-continuing-acknowledged".into())
                        .child(
                            Checkbox::new("swap-continuing-acknowledged")
                                .label(format!("Accept a delivery of {}", continuing.lower_there()))
                                .checked(form.continuing_acknowledged)
                                .small()
                                .disabled(!editable)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.continuing_acknowledged = *checked;
                                        form.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// The notice that a private Bridge delivery's Shield may be blocked, with its consent
    /// checkbox inside the warning frame. It needs no quote, and a speed-up doesn't ask again.
    /// The caller shows it only on a form that shields on another network, and names the
    /// `alternatives` that form's payer has.
    pub(super) fn render_blocked_shield_acknowledgement(
        form: &SwapForm,
        alternatives: &'static str,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if form.speed_up.is_some() {
            return None;
        }
        let warning_color = cx.theme().warning;
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(warning_color.mix_oklab(gpui::transparent_white(), 0.3))
                .bg(warning_color.mix_oklab(gpui::transparent_white(), 0.04))
                .debug_selector(|| "swap-blocked-shield-warning".into())
                .child(
                    Alert::warning("swap-blocked-shield-warning", BLOCKED_SHIELD_HEADLINE)
                        .small()
                        .min_w_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .p_0()
                        .border_0()
                        .bg(gpui::transparent_black()),
                )
                .child(
                    app_muted_text(BLOCKED_SHIELD_DETAILS)
                        .min_w_0()
                        .whitespace_normal(),
                )
                .child(app_muted_text(alternatives).min_w_0().whitespace_normal())
                .child(
                    div()
                        .debug_selector(|| "swap-blocked-shield-acknowledged".into())
                        .child(
                            Checkbox::new("swap-blocked-shield-acknowledged")
                                .label(BLOCKED_SHIELD_LABEL)
                                .checked(form.blocked_shield_acknowledged)
                                .small()
                                .disabled(!editable)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.blocked_shield_acknowledged = *checked;
                                        form.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// The wrapping warning on high authorized costs, with Swap anyway inside it.
    pub(super) fn render_cost_acknowledgement(
        form: &SwapForm,
        warning: Option<AuthorizedCostWarning>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let warning = warning?;
        // A share still being quoted shows estimated terms, which can't be accepted.
        let provisional = matches!(&form.quote, QuoteState::Ready(review)
            if form.gas_share_pending(review));
        let warning_color = cx.theme().warning;
        // One frame contains the headline, cost details, and standard checkbox.
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(warning_color.mix_oklab(gpui::transparent_white(), 0.3))
                .bg(warning_color.mix_oklab(gpui::transparent_white(), 0.04))
                .debug_selector(|| "swap-high-costs".into())
                .child(
                    div()
                        .min_w_0()
                        .debug_selector(|| "swap-high-cost-message".into())
                        .child(
                            Alert::warning("swap-high-costs", warning.headline)
                                .small()
                                .font_weight(FontWeight::SEMIBOLD)
                                .p_0()
                                .border_0()
                                .bg(gpui::transparent_black()),
                        ),
                )
                .child(
                    app_muted_text(warning.details)
                        .min_w_0()
                        .debug_selector(|| "swap-high-cost-details".into()),
                )
                .child(
                    div()
                        .debug_selector(|| "swap-costs-acknowledged".into())
                        .child(
                            Checkbox::new("swap-costs-acknowledged")
                                .label("Swap anyway")
                                .checked(form.high_costs_acknowledged)
                                .small()
                                .disabled(!editable || provisional)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.high_costs_acknowledged = *checked;
                                        form.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// Receive to, and for a Public address the Receiver with its suggestions. Under Receive
    /// to: why the form's network can't take Private balance. Under the receiver: why it can't
    /// be used, and for native output, that a contract wallet may not accept it. A retry keeps
    /// its attempt's delivery, and a set-up native pair keeps its Public address. On another
    /// network, a Public address has the Provider and Surplus rows, and Private balance, which
    /// only Across delivers, the "If the shield fails" and Surplus rows.
    pub(super) fn render_delivery(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let disabled = !editable || self.receive_to_locked(form);
        let choice = |id: &'static str, label: &'static str, receive_to: ReceiveTo| {
            app_segment_button(id, label, form.receive_to == receive_to, disabled, None).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.set_receive_to(receive_to, window, cx);
                }),
            )
        };
        let network_problem = receive_to_problem(form);
        let rows = div().w_full().flex().flex_col().gap_4().child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(labeled_row(
                    "Receive to",
                    div().flex().child(
                        ButtonGroup::new("swap-receive-to")
                            .outline()
                            .compact()
                            .disabled(disabled)
                            .child(choice(
                                "swap-receive-private",
                                "Private balance",
                                ReceiveTo::PrivateBalance,
                            ))
                            .child(choice(
                                "swap-receive-public",
                                "Public address",
                                ReceiveTo::PublicAddress,
                            )),
                    ),
                ))
                .children(network_problem),
        );
        // A set-up swap's fixed pair keeps its delivery's terms.
        let locked = form.operation.is_some() && !form.reuse_account;
        if form.receive_to == ReceiveTo::PrivateBalance {
            let across = form
                .network
                .filter(|_| matches!(form.delivery, Ok(SwapDelivery::Bridge(_))));
            return rows.when_some(across, |rows, network| {
                rows.child(self.render_shield_failure_row(form, network, editable && !locked, cx))
                    .child(self.render_surplus_row(form, editable && !locked, cx))
            });
        }
        let view = cx.entity();
        let save_root = self.root.clone();
        let rules = self.receiver_rules();
        let picker = form_recipient_picker(
            "swap-receiver-picker",
            "swap-save-receiver".into(),
            rules,
            &form.receiver_input,
            &form.receiver_value,
            form.receiver_picker.open(),
            form.receiver_picker.index(),
            form.receiver_picker.scroll(),
            &self.receiver_options(cx),
            !editable || self.is_retry(form),
            move |event, window, cx| {
                view.update(cx, |view, cx| view.receiver_picker_event(event, window, cx));
            },
            move |receiver, window, cx| {
                let _ = save_root.update(cx, |root, cx| {
                    root.open_save_recipient_dialog(rules, receiver, window, cx);
                });
            },
        );
        // An empty field only asks for an address; an entry that can't be used is an error.
        let problem = match &form.delivery {
            Err(DeliveryProblem::Receiver(problem)) => Some(
                app_muted_text(problem.clone())
                    .debug_selector(|| "swap-receiver-problem".into())
                    .text_xs()
                    .whitespace_normal()
                    .when(!form.receiver_value.trim().is_empty(), |line| {
                        line.text_color(cx.theme().danger)
                    }),
            ),
            _ => None,
        };
        let native = form.native_output.then(|| {
            div()
                .debug_selector(|| "swap-native-payout-note".into())
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Icon::new(IconName::Info)
                        .xsmall()
                        .flex_none()
                        .text_color(rgb(theme::TEXT_MUTED)),
                )
                .child(
                    app_muted_text(self.native_payout_note(cx))
                        .text_xs()
                        .whitespace_normal(),
                )
        });
        rows.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(labeled_row("Receiver", picker))
                .child(
                    // Under the field, past the label and the row's gap.
                    div()
                        .pl(rems(ACCOUNT_LABEL_WIDTH + 0.5))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(problem)
                        .children(native),
                ),
        )
        .when(form.bridged(), |rows| {
            rows.children(self.render_bridge_rows(form, editable && !locked, cx))
        })
    }

    /// Whether Across pays `token` on the form's network as its native asset: the network's
    /// configured wrapped native token reaches a receiver without code unwrapped.
    fn across_delivers_native(&self, form: &SwapForm, token: Address, cx: &App) -> bool {
        self.destination_chain(form, cx)
            .and_then(|chain| across_unwrapped_token(&chain))
            == Some(token)
    }

    /// The token a Bridge `delivery` pays out, as swaps name it: the native asset for the
    /// wrapped native token Across unwraps. A private delivery shields the wrapped token.
    pub(in crate::root::private_swap) fn bridge_received_token(
        &self,
        delivery: BridgeDelivery,
        cx: &App,
    ) -> Address {
        let unwrapped = delivery.provider == BridgeProvider::Across
            && !delivery.is_private()
            && self.root.upgrade().and_then(|root| {
                root.read(cx)
                    .effective_chain_configs
                    .get(delivery.destination_chain)
                    .and_then(across_unwrapped_token)
            }) == Some(delivery.destination_token);
        if unwrapped {
            Address::ZERO
        } else {
            delivery.destination_token
        }
    }

    /// A Bridge swap's Provider row: the select, its hint button, and the provider's refusal of
    /// the amount beneath, then Across's Surplus row.
    fn render_bridge_rows(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Vec<gpui::Div> {
        let state = form.bridge_state();
        let placeholder = match state {
            BridgeState::SameToken => "No provider for this pair",
            BridgeState::Loading => "Loading routes…",
            _ => "Choose a token to receive",
        };
        // A token only one provider delivers leaves nothing to choose.
        let alternative = matches!(
            state,
            BridgeState::Ready {
                across: true,
                near: true,
                ..
            }
        );
        // A non-EVM network has NEAR Intents alone, so the row names it without a switch.
        let select = if form.non_evm.is_some() {
            provider_mention(BridgeProvider::NearIntents, app_text)
                .debug_selector(|| "swap-provider-near-intents".into())
                .into_any_element()
        } else {
            Select::new(&form.provider_select)
                .w_full()
                .placeholder(placeholder)
                .disabled(!editable || !alternative)
                .into_any_element()
        };
        let mut rejection = None;
        let mut rows = Vec::new();
        let provider = match state {
            BridgeState::Ready { provider, .. } => Some(provider),
            BridgeState::NonEvm(_) => Some(BridgeProvider::NearIntents),
            _ => None,
        };
        if let Some(provider) = provider {
            if let QuoteState::Failed(error) = &form.quote
                && let Some(message) = bridge_rejection(error)
            {
                rejection = Some(alert_frame(
                    "swap-provider-error",
                    message,
                    None,
                    None,
                    near_intents_status(error),
                    cx,
                ));
            }
            if provider == BridgeProvider::Across {
                rows.push(self.render_surplus_row(form, editable, cx));
            }
        }
        let provider = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(labeled_row(
                "Provider",
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(select))
                    .children(self.render_provider_hint(form, cx)),
            ))
            .when_some(rejection, |row, rejection| {
                row.child(
                    // Under the select, past the label and the row's gap.
                    div().pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)).child(rejection),
                )
            });
        rows.insert(0, provider);
        rows
    }

    /// The chosen provider's hint button, right of its select: a tooltip on hover, and a
    /// popover on click or from the keyboard. The card says why the provider was switched, how
    /// it pays out, and its disclaimer. NEAR Intents' is a warning.
    fn render_provider_hint(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let (destination, provider, switched, near) = match form.bridge_state() {
            BridgeState::Ready {
                destination,
                provider,
                switched,
                near,
                ..
            } => (destination, provider, switched, near),
            // NEAR Intents is a non-EVM network's only provider: its disclaimer and no note.
            BridgeState::NonEvm(_) => {
                return Some(Self::render_provider_hint_card(
                    form,
                    BridgeProvider::NearIntents,
                    Vec::new(),
                    NEAR_INTENTS_DISCLAIMER.into(),
                    cx,
                ));
            }
            _ => return None,
        };
        let mut notes = Vec::<SharedString>::new();
        // The only provider's select is disabled, so the card says why there is no choice.
        if provider == BridgeProvider::Across && !near {
            notes.push(
                if form.bridge_unavailable() == Some(BridgeProvider::NearIntents) {
                    "NEAR Intents is unreachable, so this swap uses Across.".into()
                } else {
                    format!(
                        "NEAR Intents doesn't deliver {}, so this swap uses Across.",
                        destination.symbol
                    )
                    .into()
                },
            );
        }
        if switched {
            notes.push(
                if form.bridge_unavailable() == Some(BridgeProvider::Across) {
                    "Across is unreachable, so this swap uses NEAR Intents.".into()
                } else {
                    format!(
                        "Across doesn't deliver {}, so this swap uses NEAR Intents.",
                        destination.symbol
                    )
                    .into()
                },
            );
        }
        let disclaimer: SharedString = match provider {
            BridgeProvider::NearIntents => {
                notes.push(
                    format!(
                        "Via {} on {}. The whole payout is converted, surplus included.",
                        self.token_symbol(destination.intermediate, cx),
                        self.chain_label()
                    )
                    .into(),
                );
                NEAR_INTENTS_DISCLAIMER.into()
            }
            BridgeProvider::Across => {
                // The SpokePool unwraps the wrapped native token only for receivers without code.
                if self.across_delivers_native(form, destination.destination_token, cx) {
                    let network = form.network.unwrap_or(self.origin_chain_id);
                    notes.push(
                        format!(
                            "Across delivers {} to wallets. Contract receivers get {}.",
                            self.network_token_symbol(network, Address::ZERO, cx),
                            destination.symbol
                        )
                        .into(),
                    );
                }
                notes.push(self.surplus_note(form, destination.intermediate, cx).into());
                format!(
                    "If the deposit isn't filled before it expires, Across refunds it to the stealth account on {}, usually within a few hours. Recovering it costs a shield fee and a broadcaster fee.",
                    self.chain_label()
                )
                .into()
            }
        };
        Some(Self::render_provider_hint_card(
            form, provider, notes, disclaimer, cx,
        ))
    }

    /// [`Self::render_provider_hint`]'s button for `provider` with its `notes` and `disclaimer`.
    fn render_provider_hint_card(
        form: &SwapForm,
        provider: BridgeProvider,
        notes: Vec<SharedString>,
        disclaimer: SharedString,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let icon = match provider {
            BridgeProvider::NearIntents => {
                Icon::new(IconName::TriangleAlert).text_color(rgb(theme::WARNING))
            }
            BridgeProvider::Across => Icon::new(IconName::Info),
        };
        let view = cx.entity();
        let open = form.bridge.provider_hint_open;
        let (tooltip_notes, tooltip_disclaimer) = (notes.clone(), disclaimer.clone());
        div()
            .id("swap-provider-hint")
            .debug_selector(|| "swap-provider-hint".into())
            .flex_none()
            .when(!open, |this| {
                this.tooltip(move |window, cx| {
                    let (notes, disclaimer) = (tooltip_notes.clone(), tooltip_disclaimer.clone());
                    Tooltip::element(move |window, _| {
                        provider_hint_card(provider, &notes, &disclaimer, window)
                    })
                    .build(window, cx)
                })
            })
            .child(
                Popover::new("swap-provider-hint-popover")
                    .anchor(Anchor::TopRight)
                    .open(open)
                    .on_open_change(move |open, _, cx| {
                        view.update(cx, |view, cx| view.set_provider_hint_open(*open, cx));
                    })
                    .trigger(
                        app_button_base("swap-provider-hint-trigger")
                            .ghost()
                            .xsmall()
                            .icon(icon)
                            .accessibility_label(format!("About {}", provider_name(provider)))
                            .debug_selector(|| "swap-provider-hint-trigger".into()),
                    )
                    .content(move |_, window, _| {
                        provider_hint_card(provider, &notes, &disclaimer, window)
                            .debug_selector(|| "swap-provider-hint-content".into())
                    }),
            )
    }

    /// A private Bridge delivery's choice for a shield on `network` that can't run: refund the
    /// deposit on the swap's network, or keep the tokens in the stealth account there. The
    /// labels name the network the funds end up on, and the info button explains both.
    pub(super) fn render_shield_failure_row(
        &self,
        form: &SwapForm,
        network: u64,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let disabled = !editable || self.is_retry(form);
        let choice = |id: &'static str, failure: BridgeShieldFailure| {
            app_segment_button(
                id,
                self.form_failure_label(form, failure, network),
                form.bridge.shield_failure == failure,
                disabled,
                None,
            )
            .debug_selector(move || id.into())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_form_shield_failure(failure, window, cx);
            }))
        };
        labeled_row(
            "If the shield fails",
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(
                    ButtonGroup::new("swap-shield-failure")
                        .outline()
                        .compact()
                        .disabled(disabled)
                        .child(choice(
                            "swap-shield-failure-refund",
                            BridgeShieldFailure::RefundOnOrigin,
                        ))
                        .child(choice(
                            "swap-shield-failure-keep",
                            BridgeShieldFailure::KeepOnDestination,
                        )),
                )
                .child(self.render_shield_failure_hint(form, network, cx)),
        )
    }

    /// A failure choice as the form's control names it. A swap paid from a Public account is
    /// refunded to that account, so the refund names it instead of its network.
    pub(super) fn form_failure_label(
        &self,
        form: &SwapForm,
        choice: BridgeShieldFailure,
        network: u64,
    ) -> String {
        match (&form.public, choice) {
            (Some(public), BridgeShieldFailure::RefundOnOrigin) => {
                format!("Refund to {}", public_source_label(&public.account))
            }
            _ => shield_failure_label(choice, &self.chain_label(), &network_name(network)),
        }
    }

    /// The failure choice's hint button: a tooltip on hover, and a popover on click or from the
    /// keyboard. The card says what the choice is about and what each option does with the
    /// delivered token.
    fn render_shield_failure_hint(
        &self,
        form: &SwapForm,
        network: u64,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (origin, destination) = (self.chain_label(), network_name(network));
        let text: Arc<[(Option<String>, String)]> = if form.public.is_some() {
            Arc::from(
                self.public_failure_paragraphs(form, network, cx)
                    .map(|(label, paragraph)| (Some(label), paragraph)),
            )
        } else {
            // The deposit is refunded in the token the order buys, and delivered in the Buy
            // token.
            let (bought, delivered) = match form.bridge_state() {
                BridgeState::Ready {
                    destination: route, ..
                } => (
                    self.token_symbol(route.intermediate, cx),
                    route.symbol.clone(),
                ),
                _ => ("tokens".to_owned(), "tokens".to_owned()),
            };
            Arc::from([
                (
                    None,
                    format!(
                        "The swap shields on {destination} in the same transaction that delivers it. This choice covers the rare case where that shield can't run."
                    ),
                ),
                (
                    Some(shield_failure_label(
                        BridgeShieldFailure::RefundOnOrigin,
                        &origin,
                        &destination,
                    )),
                    format!(
                        "The delivery doesn't happen. Across returns the {bought} to the stealth account on {origin} after the deposit expires, usually within a few hours. Recovering it costs a shield fee and a broadcaster fee."
                    ),
                ),
                (
                    Some(shield_failure_label(
                        BridgeShieldFailure::KeepOnDestination,
                        &origin,
                        &destination,
                    )),
                    format!(
                        "The delivery happens, and the {delivered} stays in the stealth account on {destination}, where it is publicly visible until you recover it. Relayers price this option less precisely, so the deposit is more likely to go unfilled."
                    ),
                ),
            ])
        };
        let view = cx.entity();
        let open = form.bridge.failure_hint_open;
        let tooltip_text = Arc::clone(&text);
        div()
            .id("swap-shield-failure-hint")
            .debug_selector(|| "swap-shield-failure-hint".into())
            .flex_none()
            .when(!open, |this| {
                this.tooltip(move |window, cx| {
                    let text = Arc::clone(&tooltip_text);
                    Tooltip::element(move |window, _| shield_failure_card(&text, window))
                        .build(window, cx)
                })
            })
            .child(
                Popover::new("swap-shield-failure-hint-popover")
                    .anchor(Anchor::TopRight)
                    .open(open)
                    .on_open_change(move |open, _, cx| {
                        view.update(cx, |view, cx| view.set_failure_hint_open(*open, cx));
                    })
                    .trigger(
                        app_button_base("swap-shield-failure-hint-trigger")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Info)
                            .accessibility_label("About a shield that fails")
                            .debug_selector(|| "swap-shield-failure-hint-trigger".into()),
                    )
                    .content(move |_, window, _| {
                        shield_failure_card(&text, window)
                            .debug_selector(|| "swap-shield-failure-hint-content".into())
                    }),
            )
    }

    /// Across's choice for what `CoW` pays above the deposit: reshield it, or leave it in the
    /// stealth account. The provider's hint explains it.
    fn render_surplus_row(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let disabled = !editable || self.is_retry(form);
        let choice = |id: &'static str, label: String, surplus: BridgeSurplus| {
            app_segment_button(id, label, form.bridge.surplus == surplus, disabled, None).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.set_form_surplus(surplus, window, cx);
                }),
            )
        };
        labeled_row(
            "Surplus",
            div().flex().child(
                ButtonGroup::new("swap-bridge-surplus")
                    .outline()
                    .compact()
                    .disabled(disabled)
                    .child(choice(
                        "swap-surplus-reshield",
                        format!("Reshield on {}", self.chain_label()),
                        BridgeSurplus::Reshield,
                    ))
                    .child(choice(
                        "swap-surplus-keep",
                        "Keep in stealth account".to_owned(),
                        BridgeSurplus::KeepInAccount,
                    )),
            ),
        )
    }

    /// What the Surplus row's choice is about, with the deposit of `bought` once quoted.
    fn surplus_note(&self, form: &SwapForm, bought: Address, cx: &App) -> String {
        // Bridge orders buy exactly the deposit, the quote's minimum.
        let deposit = match &form.quote {
            QuoteState::Ready(review) if review.bridge().is_some() => format!(
                "the {} deposit",
                self.token_amount(bought, review.suggested_private_minimum(), cx)
            ),
            _ => "the deposit".to_owned(),
        };
        format!(
            "Surplus is anything CoW pays above {deposit}. Reshielding it costs the shield fee."
        )
    }

    /// `GPv2` pays a native buy with a fixed-stipend transfer, which a contract wallet whose
    /// `receive` needs more gas can't accept. The wrapped token has no such limit.
    fn native_payout_note(&self, cx: &App) -> String {
        let native = self.token_symbol(Address::ZERO, cx);
        let wrapped = self.wrapped_native_token(cx).map_or_else(
            || "its wrapped token".to_owned(),
            |token| self.token_symbol(token, cx),
        );
        format!(
            "Smart-contract wallets may not accept {native} from CoW. Choose {wrapped} for those."
        )
    }
}
