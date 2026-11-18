use super::*;

impl PrivateSwapsView {
    // Presentation helpers

    pub(in crate::root::private_swap) fn swap_profile(
        &self,
        cx: &App,
    ) -> Option<wallet_ops::settings::SwapProfile> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .swap_profile()
    }

    /// The order validity a quote requests: the swap profile's window.
    pub(super) fn default_valid_for(&self, cx: &App) -> Duration {
        self.swap_profile(cx)
            .map_or(Duration::from_mins(10), |profile| profile.valid_to_window())
    }

    pub(super) fn chain_label(&self) -> String {
        network_name(self.origin_chain_id)
    }

    pub(in crate::root::private_swap) fn token_icon(
        &self,
        token: Address,
        cx: &App,
    ) -> Option<crate::assets::WalletIconSource> {
        self.token_metadata(token, cx)
            .and_then(|metadata| metadata.icon_path)
    }

    /// "1 WETH = 2,601.30 USDC" at the quoted trading rate, excluding explicit fees.
    pub(super) fn rate_label(&self, review: &SwapReview, cx: &App) -> String {
        let plan = review.plan();
        self.pair_rate_label(
            plan.sell_token(),
            plan.buy_token(),
            review.quote().sell_amount,
            review.quote().buy_amount,
            cx,
        )
        .unwrap_or_else(|| "Unavailable".into())
    }

    /// "1 WETH = 2,601.30 USDC" for `sell_amount` of `sell` exchanged for `buy_amount` of `buy`.
    pub(in crate::root::private_swap) fn pair_rate_label(
        &self,
        sell: Address,
        buy: Address,
        sell_amount: U256,
        buy_amount: U256,
        cx: &App,
    ) -> Option<String> {
        let sell_decimals = self.token_decimals(sell, cx)?;
        let buy_decimals = self.token_decimals(buy, cx)?;
        let rate = unit_rate(sell_amount, sell_decimals, buy_amount)?;
        Some(format!(
            "1 {} = {} {}",
            self.token_symbol(sell, cx),
            railgun_ui::format_token_amount(rate, buy_decimals),
            self.token_symbol(buy, cx)
        ))
    }

    // Rendering

    /// "Speed up swap" for a second order beside an open one, "Retry swap" once an attempt of
    /// this swap ended, otherwise "Swap".
    pub(in crate::root::private_swap) fn form_title(&self) -> &'static str {
        if self
            .form
            .as_ref()
            .is_some_and(|form| form.speed_up.is_some())
        {
            "Speed up swap"
        } else if self.form.as_ref().is_some_and(|form| self.is_retry(form)) {
            "Retry swap"
        } else {
            "Swap"
        }
    }

    /// The form retries an attempt of its swap that ended. A retry keeps the attempt's tokens,
    /// stealth account and delivery; a new swap on a reused account chooses its own.
    pub(super) fn is_retry(&self, form: &SwapForm) -> bool {
        !form.reuse_account
            && form
                .operation
                .and_then(|operation| self.record(operation))
                .and_then(ExecutorRecord::swap)
                .is_some_and(|swap| !swap.orders().is_empty())
    }

    /// Receive to can't change: a retry keeps its attempt's delivery, and a set-up swap whose
    /// fixed pair buys the native asset, or bridges, can only pay a Public address, though it
    /// may name another receiver.
    pub(super) fn receive_to_locked(&self, form: &SwapForm) -> bool {
        form.public.is_some()
            || self.is_retry(form)
            || ((form.native_output || form.bridged())
                && form.operation.is_some()
                && !form.reuse_account)
    }

    /// The order detail the form returns to, when it continues or retries a swap.
    pub(in crate::root::private_swap) fn form_back_target(&self) -> Option<ExecutorOperationId> {
        self.form.as_ref()?.back_to_detail
    }

    /// The wallet's cached USD value of `amount` in micro-dollars, for display only. The price
    /// check never uses it.
    pub(in crate::root::private_swap) fn usd_micro_value(
        &self,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> Option<U256> {
        self.network_usd_micro_value(self.origin_chain_id, token, amount, cx)
    }

    pub(super) fn network_usd_micro_value(
        &self,
        network: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> Option<U256> {
        let root = self.root.upgrade()?;
        let cache = &root.read(cx).public_broadcaster_anchor_cache;
        if token == Address::ZERO {
            return cache.cached_native_usd_micro_value(network, amount);
        }
        cache.cached_token_usd_micro_value(network, token, amount)
    }

    pub(super) fn usd_label(&self, token: Address, amount: U256, cx: &App) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        Some(format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
    }

    pub(super) fn bridge_usd_value(&self, review: &SwapReview, cx: &App) -> Option<U256> {
        let SwapDelivery::Bridge(delivery) = review.plan().delivery() else {
            return None;
        };
        let bridge = review.bridge()?;
        self.network_usd_micro_value(
            delivery.destination_chain,
            delivery.destination_token,
            if bridge.private.is_some() {
                bridge.received_minimum()
            } else {
                bridge.expected_output
            },
            cx,
        )
        .or_else(|| {
            // Across delivers the same asset. Its fixed deposit less all delivery costs is
            // already in source-token units, so no destination decimals or $1 peg are assumed.
            if bridge.provider != BridgeProvider::Across {
                return None;
            }
            let received = review
                .suggested_private_minimum()
                .checked_sub(bridge_cost(review))?;
            self.usd_micro_value(review.plan().buy_token(), received, cx)
        })
    }

    pub(super) fn bridge_total_usd_value(&self, review: &SwapReview, cx: &App) -> Option<U256> {
        let surplus = review.estimated_source_surplus()?;
        let source = if surplus.is_zero() {
            U256::ZERO
        } else {
            self.usd_micro_value(review.plan().buy_token(), surplus, cx)?
        };
        Some(self.bridge_usd_value(review, cx)?.saturating_add(source))
    }

    /// `label`, which shows `amount` of `token`, followed by the amount's USD value as the
    /// wallet shows it elsewhere: "0.0025 WETH · $6.76", nothing for a dollar stablecoin, or
    /// "· USD unavailable" without a cached rate.
    pub(in crate::root::private_swap) fn with_usd(
        &self,
        label: String,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        format_value_with_usd_label(
            label,
            amount,
            self.token_decimals(token, cx),
            self.usd_micro_value(token, amount, cx),
            false,
        )
    }

    /// The USD value [`Self::with_usd`] would show for `amount` of `token`, "$6.76": none for a
    /// dollar stablecoin or without a cached rate.
    pub(in crate::root::private_swap) fn usd_value(
        &self,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        self.token_decimals(token, cx)
            .is_none_or(|decimals| {
                railgun_ui::non_redundant_usd_micro_value(amount, decimals, usd).is_some()
            })
            .then(|| railgun_ui::format_usd_micro_value(usd))
    }

    /// A gas amount of `token` in the wallet's currency, "$1.48", or as a bare amount without a
    /// cached rate.
    pub(super) fn gas_money(&self, token: Address, amount: U256, cx: &App) -> String {
        self.usd_micro_value(token, amount, cx).map_or_else(
            || self.bare_amount(token, amount, cx),
            railgun_ui::format_usd_micro_value,
        )
    }

    /// An amount the gas strip shows, without its symbol: for a Bridge `review`, in its
    /// destination token.
    pub(super) fn strip_amount(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        amount: U256,
        cx: &App,
    ) -> String {
        if let Some(delivery) = non_evm_bridge_review(review) {
            return railgun_ui::format_token_amount(amount, delivery.decimals);
        }
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                self.network_bare_amount(network, review_destination_token(review), amount, cx)
            }
            _ => self.bare_amount(review.plan().buy_token(), amount, cx),
        }
    }

    /// The symbol of the gas strip's amounts, for Custom's Minimum field.
    pub(super) fn strip_symbol(&self, form: &SwapForm, review: &SwapReview, cx: &App) -> String {
        if let Some(delivery) = non_evm_bridge_review(review) {
            return delivery.symbol;
        }
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                self.network_token_symbol(network, review_destination_token(review), cx)
            }
            _ => self.token_symbol(review.plan().buy_token(), cx),
        }
    }

    /// The decimals of the gas strip's amounts, for Custom's Minimum field.
    pub(super) fn strip_decimals(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        cx: &App,
    ) -> Option<u8> {
        if let Some(delivery) = non_evm_bridge_review(review) {
            return Some(delivery.decimals);
        }
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => self
                .chain_token_metadata(network, review_destination_token(review), cx)
                .map(|metadata| metadata.decimals),
            _ => self.token_decimals(review.plan().buy_token(), cx),
        }
    }

    /// The USD value beside Custom's Minimum field, for `amount` of the order's bought token:
    /// for a Bridge `review`, the destination amount it scales to at the destination token's
    /// rate, and none without that rate.
    pub(super) fn strip_usd_label(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        amount: U256,
        cx: &App,
    ) -> Option<String> {
        // No anchor prices a non-EVM token, so only 1Click's quote values it.
        if non_evm_bridge_review(review).is_some() {
            return review.bridge().and_then(|quote| {
                one_click_usd_estimate(quote, StripScale::of(review).show(amount))
            });
        }
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                let usd = self.network_usd_micro_value(
                    network,
                    review_destination_token(review),
                    StripScale::of(review).show(amount),
                    cx,
                )?;
                Some(format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
            }
            _ => self.usd_label(review.plan().buy_token(), amount, cx),
        }
    }

    /// The authorized share of the swap, with the gas allowance and other costs as details.
    pub(super) fn authorized_cost_warning(
        &self,
        review: &SwapReview,
        bps: u64,
        cx: &App,
    ) -> AuthorizedCostWarning {
        let buy = review.plan().buy_token();
        let gas = self.gas_money(buy, review.gas_allowance(), cx);
        // In whole percent, rounded down, as the 20% threshold reads.
        let share = format_bps_percent(bps / 100 * 100);
        let fee = bridge_fee(review);
        let delivery_cost = private_delivery_cost(review);
        let mut details = if !delivery_cost.is_zero() {
            format!(
                "Up to {gas} for source gas, a {} bridge fee, and {} for destination gas and shielding.",
                self.gas_money(buy, fee, cx),
                self.gas_money(buy, delivery_cost, cx)
            )
        } else if fee.is_zero() {
            format!("Up to {gas} for gas.")
        } else {
            format!(
                "Up to {gas} for gas and a {} bridge fee.",
                self.gas_money(buy, fee, cx)
            )
        };
        if GasBar::of(review).gas_exceeds() && review.gas_share_bps() > GAS_SHARE_TIGHT_BPS {
            details.push_str(" A higher minimum, or a larger amount, loses less.");
        }
        AuthorizedCostWarning {
            headline: format!("{share} of this swap may go to costs."),
            details,
        }
    }

    /// The warning on a quote that fell back from the form's gas share, whose gas estimate is
    /// then at least the swap's best case: the estimate, and what the user can do about it.
    pub(super) fn gas_too_high_message(
        &self,
        buy: Address,
        gas_estimate: U256,
        cx: &App,
    ) -> String {
        format!(
            "Gas for this swap costs about {} right now, more than the swap is worth. Try a larger amount, or wait for gas to drop.",
            self.gas_money(buy, gas_estimate, cx)
        )
    }

    /// The private balance of `token`, when the wallet holds any.
    pub(super) fn private_balance_label(
        &self,
        form: &SwapForm,
        token: Address,
        cx: &App,
    ) -> Option<String> {
        let (_, total) = form
            .assets
            .totals
            .iter()
            .find(|(asset, _)| *asset == token)?;
        Some(self.token_amount(token, *total, cx))
    }

    /// The private balance of `token` on `network`, another one, when the wallet holds any:
    /// "312.40 USDC private on Arbitrum One".
    pub(super) fn destination_balance_label(
        &self,
        form: &SwapForm,
        network: u64,
        token: Address,
        cx: &App,
    ) -> Option<String> {
        let (_, total) = self
            .private_totals(form, network, cx)
            .into_iter()
            .find(|(asset, _)| *asset == token)?;
        Some(format!(
            "{} private on {}",
            self.network_token_amount(network, token, total, cx),
            network_name(network)
        ))
    }

    /// The Buy asset as its button names it: the symbol its row in the Buy picker has, and its
    /// icon.
    pub(super) fn buy_token_display(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Option<(String, Option<WalletIconSource>)> {
        // A non-EVM token as 1Click lists it, the native asset with its network's icon.
        if let Some(network) = form.non_evm {
            let contract = form.non_evm_token.as_ref()?;
            return form
                .non_evm_buy_items(network)
                .into_iter()
                .find(|item| &item.contract == contract)
                .map(|item| (item.asset.label.to_string(), item.asset.icon_path));
        }
        let token = form.order_buy()?;
        let network = form.network.unwrap_or(self.origin_chain_id);
        let metadata = self.chain_token_metadata(network, token, cx);
        // A provider's token takes the provider's symbol, as its row does.
        let listed = match form.bridge_state() {
            BridgeState::Ready { destination, .. } if destination.destination_token == token => {
                Some(destination.symbol.clone())
            }
            _ => None,
        };
        let symbol = listed
            .or_else(|| metadata.as_ref().map(|metadata| metadata.symbol.clone()))
            .unwrap_or_else(|| railgun_ui::short_address(&token));
        Some((symbol, metadata.and_then(|metadata| metadata.icon_path)))
    }

    /// [`Self::bare_amount`] of a token on `network`, such as a Bridge swap's destination.
    pub(super) fn network_bare_amount(
        &self,
        network: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        self.chain_token_metadata(network, token, cx).map_or_else(
            || amount.to_string(),
            |metadata| railgun_ui::format_token_amount(amount, metadata.decimals),
        )
    }

    /// [`Self::token_amount`] of a token on `network`, such as a Bridge swap's destination.
    pub(in crate::root::private_swap) fn network_token_amount(
        &self,
        network: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        format!(
            "{} {}",
            self.network_bare_amount(network, token, amount, cx),
            self.network_token_symbol(network, token, cx)
        )
    }

    /// The lower delivery an earlier order of a speed-up `review`'s swap can still make, beside
    /// the reviewed one. `None` when no earlier approval can deliver less, as for any review
    /// other than a speed-up's.
    pub(super) fn continuing_delivery(
        &self,
        review: &SwapReview,
        cx: &App,
    ) -> Option<ContinuingDelivery> {
        let lower = review.continuing_minimum()?;
        let SwapDelivery::Bridge(delivery) = review.plan().delivery() else {
            return None;
        };
        let network = delivery.destination_chain;
        let token = self.bridge_received_token(delivery, cx);
        Some(ContinuingDelivery {
            lower: self.network_token_amount(network, token, lower, cx),
            new: self.network_token_amount(
                network,
                token,
                review.bridge()?.destination_minimum,
                cx,
            ),
            network: network_name(network),
            surplus: delivery.surplus,
        })
    }

    pub(in crate::root::private_swap) fn network_token_symbol(
        &self,
        network: u64,
        token: Address,
        cx: &App,
    ) -> String {
        self.chain_token_metadata(network, token, cx).map_or_else(
            || railgun_ui::short_address(&token),
            |metadata| metadata.symbol,
        )
    }

    /// An amount without its symbol, for a panel whose token select names the token.
    pub(in crate::root::private_swap) fn bare_amount(
        &self,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        self.token_decimals(token, cx).map_or_else(
            || amount.to_string(),
            |decimals| railgun_ui::format_token_amount(amount, decimals),
        )
    }

    /// "#181 · 0x7d20…aa10" for the swap's own stealth account.
    pub(super) fn account_label(&self, operation: ExecutorOperationId) -> Option<String> {
        let record = self.record(operation)?;
        Some(record.address().map_or_else(
            || format!("#{}", record.index()),
            |address| {
                format!(
                    "#{} · {}",
                    record.index(),
                    railgun_ui::short_address(&address)
                )
            },
        ))
    }

    /// "Setup ≈ 0.0001 WETH · $0.28 · random broadcaster", or where the estimate stands. A
    /// private Bridge swap's line gives one fee per network it sets an account up on, "Setup
    /// ≈ 0.42 DAI on Ethereum · ≈ 0.05 USDC on Arbitrum One", or names the network whose
    /// estimate it waits for. An existing account has no fee and isn't named. The flag tells
    /// that the line is a problem the user has to solve there.
    pub(super) fn setup_line(&self, form: &SwapForm, cx: &App) -> (String, bool) {
        // Only an account the swap sets up has a fee. An existing one has none.
        let sides = [SetupSide::Origin, SetupSide::Destination]
            .into_iter()
            .filter_map(|side| Some((side, self.setup_chain(form, side)?)))
            .collect::<Vec<_>>();
        if sides.is_empty() {
            return ("No setup fee. Both accounts are set up.".to_owned(), false);
        }
        if let [(SetupSide::Origin, _)] = sides.as_slice() {
            if let Some(estimate) = &form.route.estimate {
                let broadcaster = if form.route.selected.is_some() {
                    selected_broadcaster_label(&form.route.choice(), &form.route.candidates)
                } else {
                    "random broadcaster".to_owned()
                };
                let (token, fee) = (estimate.broadcaster().token, estimate.fee_amount());
                return (
                    format!(
                        "Setup ≈ {} · {broadcaster}",
                        self.with_usd(self.token_amount(token, fee, cx), token, fee, cx)
                    ),
                    false,
                );
            }
            let (status, _) = self.setup_status(&form.route, SetupSide::Origin, None, cx);
            return (format!("Setup · {status}"), false);
        }
        let fees = sides
            .iter()
            .filter_map(|(side, chain_id)| {
                let estimate = form.setup_route(*side).estimate.as_ref()?;
                Some(format!(
                    "≈ {} on {}",
                    self.chain_amount(
                        *chain_id,
                        estimate.broadcaster().token,
                        estimate.fee_amount(),
                        cx
                    ),
                    network_name(*chain_id)
                ))
            })
            .collect::<Vec<_>>();
        // The first network without an estimate says where its own stands.
        match sides
            .iter()
            .find(|(side, _)| form.setup_route(*side).estimate.is_none())
        {
            Some((side, chain_id)) => {
                let (status, problem) =
                    self.setup_status(form.setup_route(*side), *side, Some(*chain_id), cx);
                (
                    format!("Setup on {} · {status}", network_name(*chain_id)),
                    problem,
                )
            }
            None => (format!("Setup {}", fees.join(" · ")), false),
        }
    }

    /// A setup fee the form's review needs is being estimated, or its offer changed.
    pub(super) fn setup_estimate_pending(&self, form: &SwapForm) -> bool {
        [SetupSide::Origin, SetupSide::Destination]
            .into_iter()
            .any(|side| {
                self.setup_chain(form, side).is_some() && form.setup_route(side).estimate_pending()
            })
    }

    /// The line under a private swap's stealth accounts: the setup fees, or why an order waits
    /// for its destination account, which isn't confirmed as set up.
    pub(super) fn render_setup_line(&self, form: &SwapForm, cx: &App) -> gpui::Div {
        if !self.reviews_setup(form)
            && let Some(problem) = self.destination_order_problem(form)
        {
            let retry = matches!(
                self.reserved_destination_progress(form),
                Some(SwapSetupProgress::NotSent | SwapSetupProgress::Failed)
            );
            return setup_fee_line(problem, retry, !retry);
        }
        let (line, problem) = self.setup_line(form, cx);
        setup_fee_line(line, problem, self.setup_estimate_pending(form))
    }

    /// Where `route`'s estimate stands while it has none, and whether that is a problem the
    /// user has to solve. A private Bridge swap names `network`, the route's own.
    pub(super) fn setup_status(
        &self,
        route: &SetupRoute,
        side: SetupSide,
        network: Option<u64>,
        cx: &App,
    ) -> (String, bool) {
        let there = if side == SetupSide::Destination {
            " there"
        } else {
            ""
        };
        if let Some(error) = &route.estimate_error {
            // The fee token's spendable balance can't cover the fee: say so in its units.
            let status = match (
                crate::root::private_action::form_error_max_immediately_spendable(error),
                &route.estimate_candidate,
            ) {
                (Some(spendable), Some(candidate)) => format!(
                    "the fee is more than the {} you can spend{there} right now",
                    self.chain_amount(candidate.chain_id, candidate.token, spendable, cx)
                ),
                _ => error.clone(),
            };
            return (status, true);
        }
        let status = if route.estimate_task.is_some() {
            "Estimating…"
        } else if network.is_some_and(|chain_id| {
            side == SetupSide::Destination && self.destination_owner(chain_id, cx).is_none()
        }) {
            "Waiting for the network to load"
        } else if route.fee_options.is_empty() {
            "No spendable private fee token"
        } else {
            "Waiting for a compatible broadcaster"
        };
        (status.to_owned(), false)
    }

    /// [`Self::token_amount`] on the swap's own network, and [`Self::network_token_amount`] on
    /// another one.
    pub(in crate::root::private_swap) fn chain_amount(
        &self,
        chain_id: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        if chain_id == self.origin_chain_id {
            self.token_amount(token, amount, cx)
        } else {
            self.network_token_amount(chain_id, token, amount, cx)
        }
    }
}
