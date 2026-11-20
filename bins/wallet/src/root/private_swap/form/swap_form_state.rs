#[allow(clippy::wildcard_imports)]
use super::*;

impl SwapForm {
    pub(in crate::root::private_swap) const fn operation(&self) -> Option<ExecutorOperationId> {
        self.operation
    }

    /// The swap delivers on another network than its own: a chain, or a non-EVM network.
    pub(super) const fn bridged(&self) -> bool {
        self.network.is_some() || self.non_evm.is_some()
    }

    /// The form's destination network as the Buy picker keys it, `own` for the swap's own.
    pub(super) fn network_key(&self, own: u64) -> BuyNetworkKey {
        self.non_evm.map_or_else(
            || BuyNetworkKey::Chain(self.network.unwrap_or(own)),
            BuyNetworkKey::NonEvm,
        )
    }

    /// A Buy asset is chosen, on a chain or a non-EVM network.
    pub(super) const fn has_buy(&self) -> bool {
        self.buy.is_some() || self.non_evm_token.is_some()
    }

    /// The Buy list of a non-EVM `network`: 1Click's tokens there, the native asset first, then
    /// by symbol. Empty until they load. A native asset's icon is its network's.
    pub(super) fn non_evm_buy_items(&self, network: NonEvmNetwork) -> Vec<SwapBuyItem> {
        let Some(Ok(destinations)) = self.bridge.non_evm_routes.get(&(self.sell, network)) else {
            return Vec::new();
        };
        let mut items = destinations
            .iter()
            .map(|destination| SwapBuyItem {
                asset: PrivateActionAssetSelectItem {
                    token: Address::ZERO,
                    label: Arc::from(destination.symbol.as_str()),
                    icon_path: non_evm_token_icon(
                        network,
                        destination.contract.as_deref(),
                        &destination.symbol,
                    ),
                },
                near_only: false,
                contract: destination.contract.clone(),
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            (left.contract.is_some(), left.asset.label.to_lowercase())
                .cmp(&(right.contract.is_some(), right.asset.label.to_lowercase()))
        });
        items
    }

    /// The form delivers to the private balance on another network, which makes the swap a
    /// private Bridge swap with a stealth account on each network.
    pub(super) const fn private_bridge(&self) -> bool {
        matches!(self.receive_to, ReceiveTo::PrivateBalance) && self.network.is_some()
    }

    /// The form's delivery makes a cross-chain Shield whose blocked-Shield risk isn't accepted
    /// yet. A speed-up's open order already carries that delivery.
    pub(super) const fn blocked_shield_unaccepted(&self) -> bool {
        self.private_bridge() && self.speed_up.is_none() && !self.blocked_shield_acknowledged
    }

    /// Receive to or the network changed: once the delivery is no longer a private Bridge
    /// delivery, coming back to one asks for the blocked-Shield acceptance again.
    pub(super) const fn blocked_shield_delivery_changed(&mut self) {
        if !self.private_bridge() {
            self.blocked_shield_acknowledged = false;
        }
    }

    /// The existing account chosen as the private Bridge delivery's destination. A choice made
    /// for another network, or for another delivery, doesn't count.
    pub(super) fn destination_choice(&self) -> Option<DestinationAccount> {
        self.destination_account
            .filter(|account| self.private_bridge() && self.network == Some(account.chain_id))
    }

    /// The form still chooses its accounts: a new swap, or one that reuses a set-up account.
    pub(super) const fn chooses_accounts(&self) -> bool {
        self.operation.is_none() || self.reuse_account
    }

    /// Pay from can change: no stealth account is chosen or reserved, and a Public payer's
    /// swap hasn't started.
    pub(super) fn can_change_source(&self) -> bool {
        self.operation.is_none()
            && self
                .public
                .as_ref()
                .is_none_or(|public| public.operation.is_none())
    }

    /// An account choice changed: the draft claims its accounts under a new swap use while
    /// either side reuses one, and under none while both are new.
    pub(super) fn renew_reuse_use(&mut self) {
        self.reuse_use = if self.reuse_account || self.destination_choice().is_some() {
            SwapUseId::random().ok()
        } else {
            None
        };
    }

    /// The destination network changed: an account chosen on the old one doesn't follow, and
    /// the destination is a new account again. The swap's own account stays.
    pub(in crate::root::private_swap) fn clear_destination(&mut self) {
        if self.destination_account.take().is_some() {
            self.destination_route.invalidate_estimate();
            self.renew_reuse_use();
        }
    }

    /// A private delivery's terms as the form has them.
    pub(super) const fn private_delivery(&self) -> BridgePrivateDelivery {
        BridgePrivateDelivery {
            on_shield_failure: self.bridge.shield_failure,
        }
    }

    pub(super) const fn setup_route(&self, side: SetupSide) -> &SetupRoute {
        match side {
            SetupSide::Origin => &self.route,
            SetupSide::Destination => &self.destination_route,
        }
    }

    pub(super) const fn setup_route_mut(&mut self, side: SetupSide) -> &mut SetupRoute {
        match side {
            SetupSide::Origin => &mut self.route,
            SetupSide::Destination => &mut self.destination_route,
        }
    }

    /// The token the order buys: the native marker, `Address::ZERO`, for native output, and
    /// otherwise the selected Buy asset.
    pub(super) fn order_buy(&self) -> Option<Address> {
        self.buy.map(|buy| {
            if self.native_output {
                Address::ZERO
            } else {
                buy
            }
        })
    }

    /// The strip shows a gas share `review` wasn't quoted at: a Bridge swap's share, priced
    /// locally until its bridge leg is quoted again. A share the quote can't support isn't
    /// pending, as the strip then shows the quote as it was priced.
    pub(super) fn gas_share_pending(&self, review: &Arc<SwapReview>) -> bool {
        strip_review(self, review).gas_share_bps() != review.gas_share_bps()
    }

    /// The bridge leg is being quoted again at the pending share, so the amounts it sets shimmer.
    pub(super) fn bridge_refreshing(&self, review: &Arc<SwapReview>) -> bool {
        self.gas_share_pending(review) && self.bridge_quote_error.is_none()
    }

    /// `review` can't support the form's share and was priced at Tight instead. The user
    /// didn't choose that share, so the card selects no preset and shows no minimum, and
    /// [`Self::review_problem`] refuses the review until a share the quote supports is chosen.
    pub(super) fn gas_share_fallback(&self, review: &Arc<SwapReview>) -> bool {
        strip_review(self, review).gas_share_bps() != self.gas_share_bps
    }

    /// The preset the strip selects: the shown share's, or the form's until a quote is ready.
    /// None for a custom share, and none while the quote fell back from the form's share.
    pub(super) fn selected_gas_preset(
        &self,
        review: Option<&Arc<SwapReview>>,
    ) -> Option<GasPreset> {
        if self.gas_custom || review.is_some_and(|review| self.gas_share_fallback(review)) {
            return None;
        }
        GasPreset::of_share(review.map_or(self.gas_share_bps, |review| {
            strip_review(self, review).gas_share_bps()
        }))
    }

    /// The bar and the Minimum field show under the strip's row: the edit button opened them,
    /// or the share is a custom one.
    pub(super) const fn gas_bar_open(&self) -> bool {
        self.gas_minimum_editing || self.gas_custom
    }

    /// The gas share the strip shows for the ready quote, if there is one.
    pub(super) fn shown_gas_share(&self) -> Option<u16> {
        match &self.quote {
            QuoteState::Ready(review) => Some(strip_review(self, review).gas_share_bps()),
            _ => None,
        }
    }

    /// The receiver the form's quote carries: the entered one once it can be used, and until
    /// then `Address::ZERO`, a placeholder that signing refuses. A private Bridge delivery
    /// carries the placeholder until its destination stealth account is reserved.
    pub(super) const fn quote_receiver(&self) -> Address {
        match &self.delivery {
            Ok(
                SwapDelivery::External { receiver }
                | SwapDelivery::Bridge(BridgeDelivery { receiver, .. }),
            ) => *receiver,
            _ => Address::ZERO,
        }
    }

    /// The delivery to quote. No quote names the receiver, so a receiver that can't be used
    /// yet doesn't hold the quote back: the delivery then goes to the placeholder of
    /// [`Self::quote_receiver`], and [`Self::review_problem`] refuses the review. `None` while
    /// a Bridge swap has no provider for the Buy token, or its network can't take the delivery
    /// kind.
    pub(super) fn quote_delivery(&self) -> Option<SwapDelivery> {
        match &self.delivery {
            Ok(delivery) => Some(delivery.clone()),
            Err(
                DeliveryProblem::Bridge
                | DeliveryProblem::Network { .. }
                | DeliveryProblem::Account(_),
            ) => None,
            Err(DeliveryProblem::Receiver(_)) => {
                // A non-EVM quote sends 1Click a placeholder receiver of its own.
                if let (Some(network), BridgeState::NonEvm(destination)) =
                    (self.non_evm, self.bridge_state())
                {
                    return Some(SwapDelivery::NonEvmBridge(non_evm_delivery(
                        network,
                        String::new(),
                        destination,
                    )));
                }
                // Until 1Click lists the Buy token there, there is nothing to quote.
                if self.non_evm.is_some() {
                    return None;
                }
                let receiver = Address::ZERO;
                let Some(network) = self.network else {
                    return Some(SwapDelivery::External { receiver });
                };
                let terms = self.bridge_terms()?;
                Some(SwapDelivery::Bridge(BridgeDelivery {
                    provider: terms.provider,
                    destination_chain: network,
                    receiver,
                    destination_token: terms.token,
                    surplus: terms.surplus,
                    private: None,
                }))
            }
        }
    }

    /// Whether the order `review` would sign asks for less than the open order a speed-up
    /// replaces. `None` for a form that speeds nothing up.
    pub(super) fn speed_up_relaxes(&self, review: &SwapReview) -> Option<bool> {
        let open = self.speed_up.as_ref()?;
        Some(
            review
                .buy_amount_for(review.suggested_private_minimum())
                .is_ok_and(|buy_amount| swap_speed_up_relaxes(&open.bounds, buy_amount)),
        )
    }

    /// Why the form's delivery can't be quoted or reviewed.
    pub(super) fn delivery_problem(&self) -> Option<SharedString> {
        match &self.delivery {
            Err(
                DeliveryProblem::Network { problem, .. }
                | DeliveryProblem::Receiver(problem)
                | DeliveryProblem::Account(problem),
            ) => Some(problem.clone()),
            Err(DeliveryProblem::Bridge) => Some("Choose a token the bridge delivers.".into()),
            Ok(_) => None,
        }
    }

    pub(super) fn review_problem(&self, review: &Arc<SwapReview>) -> Option<SharedString> {
        match &self.delivery {
            Err(_) => self.delivery_problem(),
            Ok(delivery) if *delivery != review.plan().delivery() => {
                Some("Wait for the quote.".into())
            }
            Ok(_) if self.gas_share_pending(review) => Some(
                self.bridge_quote_error
                    .clone()
                    .unwrap_or_else(|| "Updating the bridge quote…".into()),
            ),
            Ok(_) if self.gas_share_fallback(review) => {
                Some("Gas is too high for this swap right now.".into())
            }
            Ok(_) if self.speed_up_relaxes(review) == Some(false) => {
                Some(SPEED_UP_NOT_LOWER.into())
            }
            Ok(_) if !review.price_verified() && !self.price_acknowledged => {
                Some("Accept the unverified price before you review the swap.".into())
            }
            Ok(_) if authorized_high_cost(review).is_some() && !self.high_costs_acknowledged => {
                Some("Confirm Swap anyway to accept the high swap costs.".into())
            }
            Ok(_) if review.continuing_minimum().is_some() && !self.continuing_acknowledged => {
                Some("Accept the earlier order's delivery before you review the swap.".into())
            }
            Ok(_) if self.blocked_shield_unaccepted() => Some(BLOCKED_SHIELD_PROBLEM.into()),
            Ok(_) => None,
        }
    }

    /// The token the order buys on the swap's network: for a Bridge swap, the one handed to
    /// its provider, once the provider is known.
    pub(super) fn quote_buy(&self) -> Option<Address> {
        match self.bridge_state() {
            BridgeState::SameChain => self.order_buy(),
            BridgeState::Ready { destination, .. } => Some(destination.intermediate),
            BridgeState::NonEvm(destination) => Some(destination.intermediate),
            _ => None,
        }
    }

    pub(super) fn bridge_state(&self) -> BridgeState<'_> {
        // A non-EVM network's token is listed by 1Click, which alone delivers there.
        if let Some(network) = self.non_evm {
            let destinations = match self.bridge.non_evm_routes.get(&(self.sell, network)) {
                None => return BridgeState::Loading,
                Some(Err(error)) => return BridgeState::Failed(error),
                Some(Ok(destinations)) => destinations,
            };
            let Some(token) = &self.non_evm_token else {
                return BridgeState::NoToken;
            };
            return destinations
                .iter()
                .find(|destination| &destination.contract == token)
                .map_or(BridgeState::Unavailable, BridgeState::NonEvm);
        }
        let Some(network) = self.network else {
            return BridgeState::SameChain;
        };
        let routes = match self.bridge.routes.get(&(self.sell, network)) {
            None => return BridgeState::Loading,
            Some(Err(error)) => return BridgeState::Failed(error),
            Some(Ok(routes)) => routes,
        };
        let Some(token) = self.buy else {
            return BridgeState::NoToken;
        };
        // Only Across can shield on delivery, and it shields the wrapped native token as
        // itself.
        let private = self.receive_to == ReceiveTo::PrivateBalance;
        let (across, near) = if private {
            (destination_for(&routes.across, token), None)
        } else {
            (
                routes.across_for(token),
                destination_for(&routes.near, token),
            )
        };
        let (destination, provider) = match (self.bridge.chosen, across, near) {
            (Some(BridgeProvider::NearIntents), _, Some(near)) | (_, None, Some(near)) => {
                (near, BridgeProvider::NearIntents)
            }
            (_, Some(across), _) => (across, BridgeProvider::Across),
            (_, None, None) if destination_for(&routes.same_asset, token).is_some() => {
                return BridgeState::SameToken;
            }
            (_, None, None) => {
                return match &routes.unavailable {
                    Some((provider, _)) if !private || *provider == BridgeProvider::Across => {
                        BridgeState::Unreachable(*provider)
                    }
                    _ => BridgeState::Unavailable,
                };
            }
        };
        BridgeState::Ready {
            destination,
            provider,
            across: across.is_some(),
            near: near.is_some(),
            switched: across.is_none() && self.bridge.chosen != Some(BridgeProvider::NearIntents),
        }
    }

    /// The provider the chosen network's routes couldn't ask.
    pub(super) fn bridge_unavailable(&self) -> Option<BridgeProvider> {
        let routes = self.bridge.routes.get(&(self.sell, self.network?))?;
        let (provider, _) = routes.as_ref().ok()?.unavailable.as_ref()?;
        Some(*provider)
    }

    /// The Bridge terms a quote covers, once a provider delivers the Buy token.
    pub(super) fn bridge_terms(&self) -> Option<BridgeTerms> {
        let BridgeState::Ready {
            destination,
            provider,
            ..
        } = self.bridge_state()
        else {
            return None;
        };
        Some(BridgeTerms {
            network: self.network?,
            token: destination.destination_token,
            provider,
            surplus: bridge_surplus(provider, self.bridge.surplus),
            private: self.private_bridge().then(|| self.private_delivery()),
        })
    }

    /// Use `orderbook`, or none, for this form's requests, with the bridge clients on its route.
    pub(super) fn set_orderbook(
        &mut self,
        orderbook: Option<CowOrderbookClient>,
        bridge_clients: Option<SwapBridgeClients>,
    ) {
        self.orderbook = orderbook;
        self.bridge_clients = bridge_clients.filter(|_| self.orderbook.is_some());
    }

    pub(in crate::root::private_swap) fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
}
