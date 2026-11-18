use super::*;

impl PrivateSwapsView {
    /// The stealth account select, the setup broadcaster settings for a new account, and one
    /// line on what the account choice costs or reveals.
    pub(super) fn render_account_row(
        &self,
        form: &SwapForm,
        mode: FormMode,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        // A private Bridge swap chooses an account on each network.
        if let (true, Some(source), Some(destination), Some(network)) = (
            form.private_bridge(),
            &form.account_select,
            &form.destination_select,
            form.network,
        ) {
            return self.render_account_pair(form, source, destination, network, editable, cx);
        }
        // A started private Bridge swap keeps the accounts it reserved on both networks.
        let reserved = form
            .operation
            .filter(|_| form.account_select.is_none() && form.private_bridge())
            .zip(self.reserved_delivery(form));
        // A swap paid from a Public account has one stealth account, on its destination.
        let public_select = form.public.as_ref().and(form.destination_select.as_ref());
        let control = match (&form.account_select, form.operation) {
            _ if public_select.is_some() => div()
                .children(public_select.map(|select| {
                    Select::new(select)
                        .w_full()
                        .disabled(!editable)
                        .accessibility_label("Stealth account")
                }))
                .into_any_element(),
            (Some(select), _) => Select::new(select)
                .w_full()
                .disabled(!editable)
                .into_any_element(),
            (None, Some(operation)) => match reserved {
                Some((_, delivery)) => self
                    .render_reserved_accounts(form, operation, delivery)
                    .into_any_element(),
                None => fixed_account(self.account_label(operation).unwrap_or_default())
                    .into_any_element(),
            },
            (None, None) => div().into_any_element(),
        };
        // The line under the select is secondary to it, so it's smaller.
        let line = match mode {
            _ if form.public.is_some() => self.render_public_account_line(form, cx),
            FormMode::SettingUp => self.render_setting_up_section(form),
            FormMode::Placed => app_muted_text(
                "This swap has an order. Follow it on the Private tab until it ends.",
            )
            .text_xs()
            .whitespace_normal(),
            // A new account's setup, and the one a reused account's draft still needs for
            // its new destination account. Under both reserved accounts, each of which names
            // its own reuse, the line says that neither needs one.
            FormMode::Setup { .. } | FormMode::Order
                if self.reviews_setup(form) || reserved.is_some() =>
            {
                self.render_setup_line(form, cx)
            }
            FormMode::Order if form.reuse_account => self.render_reuse_warning(form),
            FormMode::Setup { .. } | FormMode::Order => {
                app_muted_text("Already set up for this swap. No setup fee.").text_xs()
            }
        };
        let select = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    // The one stealth account select of a swap that isn't a private Bridge swap.
                    .when(form.account_select.is_some(), |control| {
                        control.debug_selector(|| "swap-account-select".into())
                    })
                    .child(control),
            )
            .when(self.reviews_setup(form), |row| {
                row.child(self.render_setup_settings(form, cx))
            })
            // An existing account has no setup to route, so the gear is off.
            .when(form.public.is_some() && !self.reviews_setup(form), |row| {
                row.child(
                    app_button_base("swap-setup-settings-off")
                        .ghost()
                        .icon(IconName::Settings)
                        .accessibility_label("Setup broadcaster")
                        .disabled(true),
                )
            });
        // The setting-up section spans the form; any other line sits under the select.
        let (column, section) = if mode == FormMode::SettingUp {
            (select, Some(line))
        } else {
            (
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(select)
                    .child(line),
                None,
            )
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                // Like `labeled_row`, but the label lines up with the select, not the line.
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap_2()
                    .child(
                        // A private Bridge swap has a stealth account on each network.
                        app_muted_text(if form.private_bridge() && form.public.is_none() {
                            "Stealth accounts"
                        } else {
                            "Stealth account"
                        })
                        .w(rems(ACCOUNT_LABEL_WIDTH))
                        .flex_none()
                        // Beside one control the label centers on it. Beside both reserved
                        // accounts it starts at their top, as it does beside both selects.
                        .when(reserved.is_none(), |label| {
                            label.h_8().flex().items_center()
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(rems(ROW_CONTROL_MIN_WIDTH))
                            .child(column),
                    ),
            )
            .children(section)
    }

    /// A private Bridge swap's two stealth account selects, the swap's own first, each under
    /// its role and network. Under a select: what reusing the chosen account reveals, with its
    /// address to copy, or why the chosen destination can't take the swap. Under both: the
    /// setup fees of the accounts the swap sets up, with their broadcaster settings. An
    /// existing account has neither.
    fn render_account_pair(
        &self,
        form: &SwapForm,
        source: &Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>,
        destination: &Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>,
        network: u64,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let side = |role: &str,
                    selector: &'static str,
                    chain_id: u64,
                    select: &Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>,
                    note: Option<gpui::AnyElement>| {
            div()
                .debug_selector(move || selector.into())
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_muted_text(format!(
                    "{role} · {}",
                    network_name(chain_id)
                )))
                .child(
                    Select::new(select)
                        .w_full()
                        .disabled(!editable)
                        .accessibility_label(format!(
                            "{role} stealth account on {}",
                            network_name(chain_id)
                        )),
                )
                .children(note)
        };
        let source_note = form
            .operation
            .filter(|_| form.reuse_account)
            .and_then(|operation| self.record(operation))
            .and_then(|record| {
                Some(
                    account_reuse_line(
                        "swap-source-reuse",
                        record.index(),
                        record.address()?,
                        self.origin_chain_id,
                    )
                    .into_any_element(),
                )
            });
        let destination_note = match (&form.delivery, form.destination_choice()) {
            (Err(DeliveryProblem::Account(problem)), _) => Some(
                div()
                    .debug_selector(|| "swap-destination-account-problem".into())
                    .child(setup_fee_line(problem.to_string(), true, false))
                    .into_any_element(),
            ),
            (_, Some(account)) => Some(
                account_reuse_line(
                    "swap-destination-reuse",
                    account.index,
                    account.address,
                    account.chain_id,
                )
                .into_any_element(),
            ),
            _ => None,
        };
        let setup = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_setup_line(form, cx)),
            )
            .when(self.reviews_setup(form), |row| {
                row.child(self.render_setup_settings(form, cx))
            });
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_2()
            .child(
                app_muted_text("Stealth accounts")
                    .w(rems(ACCOUNT_LABEL_WIDTH))
                    .flex_none(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(rems(ROW_CONTROL_MIN_WIDTH))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(side(
                        "Source",
                        "swap-account-source",
                        self.origin_chain_id,
                        source,
                        source_note,
                    ))
                    .child(side(
                        "Destination",
                        "swap-account-destination",
                        network,
                        destination,
                        destination_note,
                    ))
                    .child(setup),
            )
    }

    /// A started private Bridge swap's two stealth accounts, the swap's own first, each under
    /// its role and network as the selects are. The form can't change them: another pair
    /// needs this swap's preparation cancelled first. A reused account says what reusing it
    /// reveals, with its address to copy. `delivery` names the destination account, a new one
    /// by its stand-in until the swap reserves it.
    fn render_reserved_accounts(
        &self,
        form: &SwapForm,
        operation: ExecutorOperationId,
        delivery: BridgeDelivery,
    ) -> gpui::Div {
        let side = |role: &str,
                    selector: &'static str,
                    chain_id: u64,
                    label: String,
                    note: Option<gpui::Div>| {
            div()
                .debug_selector(move || selector.into())
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_muted_text(format!(
                    "{role} · {}",
                    network_name(chain_id)
                )))
                .child(fixed_account(label))
                .children(note)
        };
        let own = self.record(operation);
        let source_note = own
            .filter(|_| form.reuse_account)
            .and_then(|record| Some((record.index(), record.address()?)))
            .map(|(index, address)| {
                account_reuse_line("swap-source-reuse", index, address, self.origin_chain_id)
            });
        // The destination account's record, once its network is loaded, and whether the
        // swap's use reuses it.
        let swap_use = if form.reuse_account {
            form.reuse_use
        } else {
            own.and_then(ExecutorRecord::active_swap_use)
        };
        let destination = swap_use.and_then(|swap_use| {
            let account = self.swap_destination_account(
                crate::root::private_swap::model::SwapIdentity {
                    operation,
                    swap_use,
                },
                delivery,
            )?;
            let reused = account
                .swap_use(swap_use)
                .is_some_and(|claimed| !claimed.is_fresh());
            Some((account.index(), reused))
        });
        let short = railgun_ui::short_address(&delivery.receiver);
        let destination_label = match (delivery.receiver, destination) {
            (Address::ZERO, _) => NEW_ACCOUNT.to_owned(),
            (_, Some((index, _))) => format!("#{index} · {short}"),
            (_, None) => short,
        };
        let destination_note = destination.filter(|(_, reused)| *reused).map(|(index, _)| {
            account_reuse_line(
                "swap-destination-reuse",
                index,
                delivery.receiver,
                delivery.destination_chain,
            )
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(side(
                "Source",
                "swap-account-source",
                self.origin_chain_id,
                self.account_label(operation).unwrap_or_default(),
                source_note,
            ))
            .child(side(
                "Destination",
                "swap-account-destination",
                delivery.destination_chain,
                destination_label,
                destination_note,
            ))
    }

    /// The reuse warning; its tooltip holds the full privacy warning the review repeats.
    fn render_reuse_warning(&self, form: &SwapForm) -> gpui::Div {
        let account = form
            .operation
            .and_then(|operation| self.record(operation))
            .map_or_else(
                || "this account".to_owned(),
                |record| format!("#{}", record.index()),
            );
        div().child(
            div()
                .id("swap-account-reuse")
                .child(warning_line(
                    format!("Links this swap to {account}'s earlier activity. No setup fee."),
                    true,
                ))
                .tooltip(|window, cx| Tooltip::new(ACCOUNT_REUSE_NOTE).build(window, cx)),
        )
    }

    /// The gear that opens the setup broadcaster settings: fee token, Random or Specific
    /// broadcaster, favorites only, and out-of-range fees. A private Bridge swap has them once
    /// per network, each under its network's name.
    fn render_setup_settings(&self, form: &SwapForm, cx: &Context<'_, Self>) -> Popover {
        let view = cx.entity();
        let open_view = view.clone();
        let busy = self.busy();
        let routes = [SetupSide::Origin, SetupSide::Destination]
            .into_iter()
            .filter_map(|side| {
                let chain_id = self.setup_chain(form, side)?;
                Some((
                    side,
                    network_name(chain_id),
                    SetupRouteSettings::of(form.setup_route(side), busy),
                ))
            })
            .collect::<Vec<_>>();
        let two_networks = routes.len() > 1;
        let label = if two_networks {
            "Setup broadcasters"
        } else {
            "Setup broadcaster"
        };
        Popover::new("swap-setup-settings")
            .anchor(Anchor::TopRight)
            // Presses elsewhere in the form close it; see `render_form`.
            .overlay_closable(false)
            .open(form.settings_open)
            .on_open_change(move |open, _, cx| {
                open_view.update(cx, |view, cx| view.set_setup_settings_open(*open, cx));
            })
            .trigger(
                app_button_base("swap-setup-settings-trigger")
                    .ghost()
                    .icon(IconName::Settings)
                    .accessibility_label(label)
                    .tooltip(label),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(24.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(two_networks, |content| {
                        content.child(app_strong_text("Setup fees"))
                    })
                    .children(routes.iter().map(|(side, network, route)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .when(two_networks, |settings| {
                                settings.child(app_muted_text(network.clone()).text_xs())
                            })
                            .child(setup_route_settings(view.clone(), *side, route))
                    }))
                    .when(two_networks, |content| {
                        content.child(
                            app_muted_text(
                                "Each account is set up by a broadcaster on its own network, paid from your private balance there.",
                            )
                            .text_xs()
                            .whitespace_normal(),
                        )
                    })
            })
    }

    /// A swap's estimated costs, "≈ $0.12", then `suffix`: in money, or as the token amount
    /// without a cached rate.
    pub(super) fn costs_label(&self, review: &SwapReview, suffix: &str, cx: &App) -> String {
        let buy = review.plan().buy_token();
        let costs = total_cost(review);
        self.usd_micro_value(buy, costs, cx).map_or_else(
            || {
                self.with_usd(
                    format!("≈ {}{suffix}", self.token_amount(buy, costs, cx)),
                    buy,
                    costs,
                    cx,
                )
            },
            |usd| format!("≈ {}{suffix}", railgun_ui::format_usd_micro_value(usd)),
        )
    }
}
