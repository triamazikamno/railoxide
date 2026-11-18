use super::*;

impl PrivateSwapsView {
    /// The Buy card's label. A same-token deposit is received as it is, so its card reads
    /// "Receive". An order's amount, once `quoted`, is a minimum, which the label says.
    pub(in crate::root::private_swap::form) fn public_buy_label(
        form: &SwapForm,
        quoted: bool,
    ) -> String {
        let deposit = Self::public_form_path(form) == Some(PublicBridgePath::Deposit);
        match form.network.map(network_name) {
            None => "Buy on another network".to_owned(),
            Some(network) if deposit => format!("Receive on {network}"),
            Some(network) if quoted => format!("Buy on {network}, at least"),
            Some(network) => format!("Buy on {network}"),
        }
    }

    /// The path the form's pair takes: its review's, or before one its route's.
    pub(in crate::root::private_swap::form) fn public_form_path(
        form: &SwapForm,
    ) -> Option<PublicBridgePath> {
        let public = form.public.as_ref()?;
        public
            .review
            .as_ref()
            .map(|review| review.path())
            .or_else(|| public_route(form).map(|route| route.path))
    }

    /// The Public account's native balance and what `review` needs of it, when it has less:
    /// its gas maximum, and the amount of a native Sell asset. `None` too while the balance
    /// isn't read.
    pub(super) fn public_native_shortfall(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<(U256, U256)> {
        let public = form.public.as_ref()?;
        let required =
            review
                .gas_plan()
                .max_gas_cost
                .saturating_add(if form.sell == Address::ZERO {
                    review.sell_amount()
                } else {
                    U256::ZERO
                });
        if required.is_zero() {
            return None;
        }
        let root = self.root.upgrade()?;
        // A balance that wasn't read isn't a shortfall: the reason states what the account has.
        let held = crate::root::public_balances::public_account_balances_for_chain(
            root.read(cx).public_balance_snapshot.as_deref(),
            self.origin_chain_id,
            &public.account.public_account_uuid,
            PublicAccountStatus::Active,
        )?
        .iter()
        .find(|balance| balance.asset.id == wallet_ops::PublicAssetId::Native)?
        .amount
        .amount()?;
        (held < required).then_some((required, held))
    }

    /// "Main needs 0.0004 ETH on Ethereum for gas and has 0.0001."
    pub(super) fn public_shortfall_text(
        &self,
        form: &SwapForm,
        (required, held): (U256, U256),
        cx: &App,
    ) -> String {
        format!(
            "{} needs {} on {} for {} and has {}.",
            form.public
                .as_ref()
                .map_or_else(String::new, |public| public_source_label(&public.account)),
            self.token_amount(Address::ZERO, required, cx),
            self.chain_label(),
            if form.sell == Address::ZERO {
                "the amount and gas"
            } else {
                "gas"
            },
            self.bare_amount(Address::ZERO, held, cx)
        )
    }

    /// "Main's `CoW` proxy 0x5cD8…a310 holds 994.47 USDC. Withdraw it to Main first, then review
    /// the swap again. No order was placed."
    pub(super) fn public_proxy_holds_text(
        &self,
        command: &PublicSwapAuthorization,
        proxy: Address,
        balance: Option<U256>,
        cx: &App,
    ) -> String {
        let label = public_source_label(&command.source);
        let held = match &command.action {
            PublicSwapCommand::Swap { review, .. } => {
                let token = review.intent().bridged_token;
                balance.map_or_else(
                    || format!("an unreadable balance of {}", self.token_symbol(token, cx)),
                    |balance| self.token_amount(token, balance, cx),
                )
            }
            PublicSwapCommand::Withdraw { .. } | PublicSwapCommand::Cancel { .. } => {
                "an unreadable balance".to_owned()
            }
        };
        format!(
            "{label}'s CoW proxy {} holds {held}. Withdraw it to {label} first, then review the swap again. No order was placed.",
            railgun_ui::short_address(&proxy)
        )
    }

    /// Why the form can't quote or be reviewed, one reason at a time: an open swap the
    /// backend refused the pair for, a native Sell asset that would need a swap, a destination
    /// whose private balance isn't ready, a pair no route serves, an amount too small to
    /// bridge, and a Public account short of gas. Informational only: the wrap suggestion and
    /// a network without swaps.
    pub(in crate::root::private_swap::form) fn public_form_reason(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Option<PublicFormReason> {
        let public = form.public.as_ref()?;
        let label = public_source_label(&public.account);
        if let Some(conflict) = public
            .conflict
            .filter(|conflict| conflict.sell == form.sell && conflict.buy == form.buy)
        {
            let token = self.token_symbol(conflict.token, cx);
            let until = conflict.until.map(local_time_label);
            let text = match (conflict.buys, until) {
                (true, Some(until)) => format!(
                    "{label} already has an open swap that buys {token}. It can buy {token} again from {until}."
                ),
                (true, None) => format!("{label} already has an open swap that buys {token}."),
                (false, Some(until)) => format!(
                    "{label} already has an open order that sells {token}. It expires at {until}."
                ),
                (false, None) => {
                    format!("{label} already has an open order that sells {token}.")
                }
            };
            return Some(PublicFormReason {
                text,
                blocks_review: true,
                retry_routes: false,
                open: self.public_records.iter().find_map(|(_, record)| {
                    record.public_swap_use(conflict.swap).map(|_| {
                        crate::root::private_swap::model::SwapIdentity {
                            operation: record.operation(),
                            swap_use: conflict.swap,
                        }
                    })
                }),
            });
        }
        let root = self.root.upgrade()?;
        let origin = root
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)
            .cloned()?;
        let blocking = |text: String| PublicFormReason {
            text,
            blocks_review: true,
            open: None,
            retry_routes: false,
        };
        // The routes of the form's sell token to its network, once they are read, and whether
        // one of them delivers the picked Buy token.
        let pair = form.network.zip(form.buy);
        let routes = form
            .network
            .and_then(|network| public.routes.get(&(form.sell, network)));
        let unrouted = pair.zip(routes).is_some_and(|((_, buy), routes)| {
            !routes
                .iter()
                .any(|route| route.destination.destination_token == buy)
        });
        // Where orders aren't available, by the chain's profile or because its math contract
        // isn't deployed, only deposits are offered.
        let can_swap = Self::public_can_swap(&origin, public);
        // A native Sell asset is only deposited, as its wrapped token: no order sells it.
        let native = form.sell == Address::ZERO && can_swap;
        let wrap = |advice: &str| {
            format!(
                "{} can't be swapped from a Public account{advice}",
                origin.native_currency.symbol
            )
        };
        let wrapped = origin.wrapped_native_token.map_or_else(
            || "its wrapped token".to_owned(),
            |token| self.token_symbol(token, cx),
        );
        if native && unrouted {
            return Some(blocking(wrap(&format!(
                ". Wrap it first, or pick {wrapped} on the destination to bridge it."
            ))));
        }
        if let Some((network, buy)) = pair {
            if let Some(problem) = self.public_destination_problem(network, cx) {
                return Some(blocking(problem));
            }
            if unrouted {
                return Some(blocking(format!(
                    "Across doesn't deliver {} on {} for {} from {}. Pick another token.",
                    self.network_token_symbol(network, buy, cx),
                    network_name(network),
                    self.form_sell_metadata(form, cx).map_or_else(
                        || self.token_symbol(form.sell, cx),
                        |metadata| metadata.symbol
                    ),
                    self.chain_label()
                )));
            }
            if routes.is_none()
                && let Some(error) = public.route_errors.get(&(form.sell, network))
            {
                return Some(PublicFormReason {
                    text: format!(
                        "Couldn't read what Across delivers on {}. {error}",
                        network_name(network)
                    ),
                    blocks_review: true,
                    open: None,
                    retry_routes: true,
                });
            }
        }
        // Across's least deposit with the shield's gas, estimated from the review's own quote.
        if let Some((review, network)) = public.review.as_ref().zip(form.network)
            && let Some(floor) = review.too_small_to_bridge()
        {
            let network = network_name(network);
            // An order's floor is in the bridged token, which the form doesn't show.
            return Some(blocking(match review.path() {
                PublicBridgePath::Deposit => format!(
                    "{} is too small to bridge to your private balance on {network}. The shield's gas there would take too large a share. Try at least about {}.",
                    self.form_sell_amount(form, review.sell_amount(), cx),
                    self.form_sell_amount(form, floor, cx)
                ),
                PublicBridgePath::Order => format!(
                    "This amount is too small to bridge to your private balance on {network}. The shield's gas there would take too large a share. Try a larger amount."
                ),
            }));
        }
        if let Some(shortfall) = public
            .review
            .as_ref()
            .and_then(|review| self.public_native_shortfall(form, review, cx))
        {
            return Some(blocking(self.public_shortfall_text(form, shortfall, cx)));
        }
        // No Buy token is picked yet: the suggestion only informs. A picked one that a deposit
        // serves needs none.
        if native && form.buy.is_none() {
            return Some(PublicFormReason {
                text: wrap(&format!(
                    ". Wrap it first, or pick {wrapped} on the destination to bridge it."
                )),
                blocks_review: false,
                open: None,
                retry_routes: false,
            });
        }
        // Said only once it is known: a chain not read yet may still take orders.
        let no_orders =
            origin.public_swap_profile().is_none() || public.orders_available == Some(false);
        (origin.bridge_origin_profile().is_some() && no_orders).then(|| PublicFormReason {
            text: format!(
                "From {}, a token can only be bridged as itself, such as USDC to USDC. Swapping to a different token isn't supported on this network.",
                self.chain_label()
            ),
            blocks_review: false,
            open: None,
            retry_routes: false,
        })
    }

    /// Why the private balance on `network` can't take the delivery yet, which keeps the form
    /// from quoting: the conditions of [`Self::ready_destination`], read without loading
    /// anything.
    fn public_destination_problem(&self, network: u64, cx: &App) -> Option<String> {
        // The screenshot fixture holds a route failure without a live destination session.
        // Review still uses ready_destination; this exemption only exposes the error action.
        #[cfg(debug_assertions)]
        if ui_fixture::route_error() {
            return None;
        }
        let name = network_name(network);
        let root = self.root.upgrade()?;
        Some(match root.read(cx).chain_states.get(&network) {
            Some(ChainUtxoState::Ready { session, .. }) => {
                if session.executor_owner().is_some() {
                    return None;
                }
                format!("Stealth accounts aren't available on {name} for this wallet.")
            }
            // A network that was ready is syncing again, as it does from time to time.
            Some(ChainUtxoState::Syncing { session, .. })
                if self.public_synced.contains(&network) && session.executor_owner().is_some() =>
            {
                return None;
            }
            Some(ChainUtxoState::Error { .. }) => format!(
                "Private balance needs {name} synced, and its sync failed. Pick a token on another network."
            ),
            _ => format!(
                "Private balance needs {name} synced first. Wait for it, or pick a token on another network."
            ),
        })
    }

    /// A reason under the Sell card, with "Open it…" for the open swap it names.
    pub(in crate::root::private_swap::form) fn render_public_form_reason(
        &self,
        reason: PublicFormReason,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let open = reason.open.map(|identity| {
            app_button("swap-public-open-swap", "Open it…")
                .debug_selector(|| "swap-public-open-swap".into())
                .outline()
                .small()
                .flex_none()
                .disabled(self.busy())
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.navigate(SwapDialogView::PublicDetail(identity), window, cx);
                }))
        });
        let line = div()
            .debug_selector(|| "swap-public-reason".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2();
        if reason.blocks_review {
            if reason.retry_routes {
                return line.child(action_alert(
                    "swap-public-reason-text",
                    reason.text,
                    self.bridge_routes_retry(cx),
                    cx,
                ));
            }
            return line.child(match open {
                Some(open) => action_alert("swap-public-reason-text", reason.text, open, cx),
                None => error_alert("swap-public-reason-text", reason.text),
            });
        }
        line.child(
            // A limit of the source, not a problem: it keeps nothing from being reviewed.
            div()
                .debug_selector(|| "swap-public-reason-text".into())
                .flex_1()
                .min_w_0()
                .child(
                    Alert::warning("swap-public-reason-text", reason.text)
                        .small()
                        .min_w_0(),
                ),
        )
        .children(open)
    }

    /// The line under the stealth account select: what reusing the chosen account links, a
    /// new account's setup fee and where it is paid from, or that a continued swap's account
    /// is on its way.
    pub(in crate::root::private_swap::form) fn render_public_account_line(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> gpui::Div {
        if let Some(account) = form.destination_choice() {
            return reuse_line(
                "swap-destination-reuse",
                format!(
                    "Reuses #{} on {} and links this swap to its earlier activity.",
                    account.index,
                    network_name(account.chain_id)
                ),
                account.address,
            );
        }
        // Without a network there is no account to describe yet.
        if form.network.is_none() {
            return div();
        }
        if self.destination_setup_chain(form).is_some() {
            // Debug UI fixture: its stand-in setup fee, as an estimate's line reads.
            if let Some(setup) = self.ui_fixture_setup(form, cx) {
                return setup_fee_line(
                    format!(
                        "Setup ≈ {} on {}, from your private balance there",
                        self.chain_amount(setup.chain_id, setup.token, setup.maximum, cx),
                        network_name(setup.chain_id)
                    ),
                    false,
                    false,
                );
            }
            let (line, problem) = self.setup_line(form, cx);
            let line = if !problem && form.destination_route.estimate.is_some() {
                format!("{line}, from your private balance there")
            } else {
                line
            };
            return setup_fee_line(line, problem, self.setup_estimate_pending(form));
        }
        app_muted_text(match self.public_destination_setup_progress(form) {
            Some(SwapSetupProgress::Done) | None => "Already set up for this swap. No setup fee.",
            Some(_) => {
                "Setting up the swap's stealth account · Waiting for the setup to be confirmed"
            }
        })
        .text_xs()
        .whitespace_normal()
    }

    /// The two failure choices of a swap paid from a Public account, each with what it does
    /// with the deposit: refunded to the account in the token it deposited, or kept on
    /// `network` in the token delivered there.
    pub(in crate::root::private_swap::form) fn public_failure_paragraphs(
        &self,
        form: &SwapForm,
        network: u64,
        cx: &App,
    ) -> [(String, String); 2] {
        let (origin, destination) = (self.chain_label(), network_name(network));
        let label = form
            .public
            .as_ref()
            .map_or_else(String::new, |public| public_source_label(&public.account));
        let (deposited, delivered) = public_route(form).map_or_else(
            || ("tokens".to_owned(), "tokens".to_owned()),
            |route| {
                (
                    self.token_symbol(route.destination.intermediate, cx),
                    route.destination.symbol.clone(),
                )
            },
        );
        [
            (
                self.form_failure_label(form, BridgeShieldFailure::RefundOnOrigin, network),
                format!(
                    "The delivery doesn't happen. Across returns the {deposited} to {label} on {origin} after the deposit expires, usually within a few hours. Nothing needs recovering."
                ),
            ),
            (
                self.form_failure_label(form, BridgeShieldFailure::KeepOnDestination, network),
                format!(
                    "The delivery happens and the {delivered} stays in the stealth account on {destination}, publicly visible, until you recover it there. Relayers price this option less precisely, so the deposit is more likely to go unfilled."
                ),
            ),
        ]
    }
}
