use super::*;

impl PrivateSwapsView {
    pub(in crate::root::private_swap) fn sell_assets(
        &self,
        operation: Option<ExecutorOperationId>,
        cx: &App,
    ) -> Vec<UnshieldAsset> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.origin_chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        let wrapped = root
            .effective_chain_configs
            .get(self.origin_chain_id)
            .and_then(|chain| chain.wrapped_native_token);
        // Debug UI fixture: a `max:` mode sells a stand-in when the wallet holds nothing to swap.
        let fixture_assets = || {
            wrapped
                .and_then(|wrapped| {
                    crate::root::private_swap::ui_fixture::max_asset(self.origin_chain_id, wrapped)
                })
                .map_or_else(Vec::new, |asset| vec![asset])
        };
        let (Some(owner), Some(session)) = (self.private_owner(), self.private_session()) else {
            return fixture_assets();
        };
        let mut assets = root
            .private_action_asset_options(DeliveryFormKind::Unshield, self.origin_chain_id)
            .into_iter()
            .filter(|asset| {
                profile.token_eligibility(asset.token, &SwapTokenRole::Sell)
                    == SwapTokenEligibility::Eligible
            })
            .collect::<Vec<_>>();
        let tokens = assets.iter().map(|asset| asset.token).collect::<Vec<_>>();
        let maxima = owner
            .max_swap_amounts(session, operation, &tokens)
            .unwrap_or_else(|_| vec![U256::ZERO; tokens.len()]);
        for (asset, max) in assets.iter_mut().zip(maxima) {
            asset.max_batched = max;
        }
        if assets.is_empty() {
            return fixture_assets();
        }
        // The wrapped native token leads; the rest keep their order.
        assets.sort_by_key(|asset| native_rank(asset.token, wrapped));
        assets
    }

    /// Read the form's [`FormAssets`] again.
    pub(in crate::root::private_swap) fn refresh_form_assets(&mut self, cx: &App) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let assets = match &form.public {
            Some(public) => self.public_form_assets(&public.account, form.sell, cx),
            None => self.form_assets(form.operation, form.sell, cx),
        };
        if let Some(form) = self.form.as_mut() {
            form.assets = assets;
        }
    }

    pub(super) fn form_assets(
        &self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        cx: &App,
    ) -> FormAssets {
        let totals = self.root.upgrade().map_or_else(Vec::new, |root| {
            root.read(cx)
                .private_action_asset_options(DeliveryFormKind::Unshield, self.origin_chain_id)
                .into_iter()
                .map(|asset| (asset.token, asset.total))
                .collect()
        });
        FormAssets {
            sell_assets: self.sell_assets(operation, cx),
            totals,
            // Debug UI fixture: `max:locked` locks part of its stand-in Sell asset.
            locked: self
                .private_session()
                .map(|session| session.locked_note_value(sell))
                .filter(|locked| !locked.is_zero())
                .or_else(|| {
                    crate::root::private_swap::ui_fixture::locked_sell_value(
                        sell,
                        self.wrapped_native_token(cx),
                    )
                })
                .unwrap_or_default(),
            // The Buy list leaves out the token sold. Its configured tokens never hold the native
            // asset, which the picker adds beside the wrapped native token.
            sell_receivable: self
                .buy_items(Address::ZERO, cx)
                .iter()
                .any(|item| item.token == sell),
        }
    }

    /// The destination list: configured tokens the swap profile accepts, the wrapped native
    /// token first. The native asset a Public address can take instead is added by
    /// [`Self::buy_items_on`].
    fn buy_items(&self, sell: Address, cx: &App) -> Vec<PrivateActionAssetSelectItem> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.origin_chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        let wrapped = root
            .effective_chain_configs
            .get(self.origin_chain_id)
            .and_then(|chain| chain.wrapped_native_token);
        let mut items = swap_destination_tokens(&root.effective_token_registry, &profile)
            .into_iter()
            .filter_map(|info| {
                let token = info.token_address.parse::<Address>().ok()?;
                (token != sell).then(|| PrivateActionAssetSelectItem {
                    token,
                    label: Arc::from(info.symbol.as_str()),
                    icon_path: self.token_icon(token, cx),
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            (native_rank(left.token, wrapped), &left.label)
                .cmp(&(native_rank(right.token, wrapped), &right.label))
        });
        items
    }

    /// A Bridge swap's destination list: what either provider delivers, the native asset first
    /// and `network`'s wrapped native token next. Tokens
    /// only NEAR Intents delivers carry its tag. The wrapped native token Across unwraps is
    /// listed as the native asset. The sell token's own asset stays listed, so picking it
    /// explains why the pair isn't bridged. A `private` delivery lists only what Across
    /// delivers, the wrapped native token as itself, and never the sell token's own asset.
    fn bridge_buy_items(
        &self,
        network: u64,
        routes: &BridgeRoutes,
        private: bool,
        cx: &App,
    ) -> Vec<SwapBuyItem> {
        let mut items: Vec<SwapBuyItem> = Vec::new();
        let across = routes.across.iter().map(|destination| {
            let token = if private {
                destination.destination_token
            } else {
                routes.across_buy_token(destination)
            };
            (token, destination, false)
        });
        let others = routes
            .near
            .iter()
            .map(|destination| (destination.destination_token, destination, true))
            .chain(
                routes
                    .same_asset
                    .iter()
                    .map(|destination| (destination.destination_token, destination, false)),
            )
            .filter(|_| !private);
        for (token, destination, near_only) in across.chain(others) {
            if items.iter().any(|item| item.asset.token == token) {
                continue;
            }
            let metadata = self.chain_token_metadata(network, token, cx);
            // Across's unwrapped token takes the native asset's name.
            let label = match &metadata {
                Some(metadata) if token != destination.destination_token => &metadata.symbol,
                _ => &destination.symbol,
            };
            items.push(SwapBuyItem {
                asset: PrivateActionAssetSelectItem {
                    token,
                    label: Arc::from(label.as_str()),
                    icon_path: metadata.and_then(|metadata| metadata.icon_path),
                },
                near_only,
                contract: None,
            });
        }
        let wrapped = self.root.upgrade().and_then(|root| {
            root.read(cx)
                .effective_chain_configs
                .get(network)?
                .wrapped_native_token
        });
        items.sort_by(|left, right| {
            (native_rank(left.asset.token, wrapped), &left.asset.label)
                .cmp(&(native_rank(right.asset.token, wrapped), &right.asset.label))
        });
        items
    }

    /// The Buy list of `network`, or of the swap's own, for the form's sell token and delivery
    /// kind. Another network's is empty until its routes load.
    pub(super) fn buy_items_on(
        &self,
        form: &SwapForm,
        network: Option<u64>,
        cx: &App,
    ) -> Vec<SwapBuyItem> {
        if let Some(public) = &form.public {
            let Some(network) = network else {
                return Vec::new();
            };
            return public
                .routes
                .get(&(form.sell, network))
                .into_iter()
                .flatten()
                .map(|route| SwapBuyItem {
                    asset: PrivateActionAssetSelectItem {
                        token: route.destination.destination_token,
                        label: Arc::from(route.destination.symbol.as_str()),
                        icon_path: self
                            .chain_token_metadata(network, route.destination.destination_token, cx)
                            .and_then(|metadata| metadata.icon_path),
                    },
                    near_only: false,
                    contract: None,
                })
                .collect();
        }
        match network {
            None => {
                let mut items = self.buy_items(form.sell, cx);
                // A Public address can take the listed wrapped native token as the native asset,
                // which leads the list.
                if form.receive_to == ReceiveTo::PublicAddress
                    && let Some(wrapped) = self.wrapped_native_token(cx)
                    && items.iter().any(|item| item.token == wrapped)
                    && let Some(native) =
                        self.chain_token_metadata(self.origin_chain_id, Address::ZERO, cx)
                {
                    items.insert(
                        0,
                        PrivateActionAssetSelectItem {
                            token: Address::ZERO,
                            label: Arc::from(native.symbol.as_str()),
                            icon_path: native.icon_path,
                        },
                    );
                }
                same_chain_buy_items(items)
            }
            Some(network) => match form.bridge.routes.get(&(form.sell, network)) {
                Some(Ok(routes)) => self.bridge_buy_items(
                    network,
                    routes,
                    form.receive_to == ReceiveTo::PrivateBalance,
                    cx,
                ),
                _ => Vec::new(),
            },
        }
    }

    /// The networks `receive_to` can deliver on, as `network_list` lists them.
    pub(super) fn network_items(&self, receive_to: ReceiveTo, cx: &App) -> Vec<BuyNetwork> {
        self.network_list(receive_to, cx).0
    }

    /// The networks `receive_to` can deliver on: this one first, then the other swap chains and
    /// every other chain enabled with RPC endpoints, in the network selector's order. One that
    /// fails a condition of the delivery kind is listed with the reason and can't be picked,
    /// unless its sync or setup-fee check is still under way. Private balance leaves out the
    /// chains without a private balance, and the count of them is returned with the list.
    pub(super) fn network_list(
        &self,
        receive_to: ReceiveTo,
        cx: &App,
    ) -> (Vec<BuyNetwork>, (usize, usize)) {
        let own = self.origin_chain_id;
        let public = self.form.as_ref().is_some_and(|form| form.public.is_some());
        let mut items = vec![BuyNetwork {
            key: BuyNetworkKey::Chain(own),
            label: network_name(own).into(),
            this_network: true,
            availability: if public {
                NetworkAvailability::Unavailable(NetworkUnavailable::Shield)
            } else {
                NetworkAvailability::Available
            },
            reusable: false,
        }];
        let Some(root) = self.root.upgrade() else {
            return (items, (1, 1));
        };
        let root = root.read(cx);
        // How many networks each delivery kind lists, for its switch.
        let mut private = usize::from(
            root.effective_chain_configs
                .get(own)
                .is_some_and(|chain| chain.swap_profile().is_some()),
        );
        // This network is listed for a Public address whether or not it has a private balance.
        let own_public_only = usize::from(private == 0);
        let bridge_available = root.effective_chain_configs.get(own).is_some_and(|chain| {
            if public {
                chain.bridge_origin_profile().is_some()
            } else {
                chain.bridge_profile().is_some()
            }
        });
        if !bridge_available {
            return (items, (private, 1));
        }
        let mut public_only = 0;
        items.extend(
            root.effective_chain_configs
                .values()
                .filter(|chain| {
                    chain.chain_id != own
                        && (chain.bridge_profile().is_some()
                            || chain.bridge_destination().is_some())
                })
                .filter(|chain| {
                    // Private balance leaves out the chains that take a Public address only.
                    let takes_private = chain.swap_profile().is_some();
                    if takes_private {
                        private += 1;
                    } else {
                        public_only += 1;
                    }
                    receive_to == ReceiveTo::PublicAddress || takes_private
                })
                .map(|chain| BuyNetwork {
                    key: BuyNetworkKey::Chain(chain.chain_id),
                    label: network_name(chain.chain_id).into(),
                    this_network: false,
                    availability: self
                        .checked_destination_availability(root, receive_to, chain, cx),
                    reusable: self.destination_reusable(chain.chain_id),
                }),
        );
        // Both delivery kinds list the other networks in the network selector's order.
        items[1..].sort_by_key(|network| {
            network
                .key
                .chain_id()
                .map(ui::chain_select::chain_select_order)
        });
        // NEAR Intents delivers to the non-EVM networks from any swap chain with a 1Click name,
        // which a bridge profile has. They take a Public address only, and come last.
        let non_evm = if public { 0 } else { NonEvmNetwork::ALL.len() };
        if !public && receive_to == ReceiveTo::PublicAddress {
            items.extend(NonEvmNetwork::ALL.map(|network| BuyNetwork {
                key: BuyNetworkKey::NonEvm(network),
                label: network.display_name().into(),
                this_network: false,
                availability: NetworkAvailability::Available,
                reusable: false,
            }));
        }
        (
            items,
            (private, private + public_only + own_public_only + non_evm),
        )
    }

    /// Whether `receive_to` can deliver on `network`, another chain. `None` when the picker
    /// doesn't list it from the swap's chain.
    pub(super) fn network_availability(
        &self,
        receive_to: ReceiveTo,
        network: u64,
        cx: &App,
    ) -> Option<NetworkAvailability> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        let public = self.form.as_ref().is_some_and(|form| form.public.is_some());
        let origin = root.effective_chain_configs.get(self.origin_chain_id)?;
        if public {
            origin.bridge_origin_profile()?;
            if network == self.origin_chain_id {
                return Some(NetworkAvailability::Unavailable(NetworkUnavailable::Shield));
            }
        } else {
            origin.bridge_profile()?;
        }
        let chain = root.effective_chain_configs.get(network)?;
        if chain.bridge_profile().is_none() && chain.bridge_destination().is_none() {
            return None;
        }
        Some(self.checked_destination_availability(root, receive_to, chain, cx))
    }

    fn checked_destination_availability(
        &self,
        root: &WalletRoot,
        receive_to: ReceiveTo,
        chain: &EffectiveChainConfig,
        cx: &App,
    ) -> NetworkAvailability {
        let reusable = self.destination_reusable(chain.chain_id);
        match destination_availability(root, receive_to, chain, reusable) {
            NetworkAvailability::Available if receive_to == ReceiveTo::PrivateBalance => {
                match self.network_setup_availability(chain.chain_id, cx) {
                    // The estimate shows that setup can't be funded, which leaves a set-up
                    // account there to select.
                    NetworkAvailability::Unavailable(NetworkUnavailable::Unfunded) if reusable => {
                        NetworkAvailability::ReuseOnly
                    }
                    availability => availability,
                }
            }
            // Routes are fetched for a network once the picker shows it, so only then is it
            // known that no provider serves it.
            NetworkAvailability::Available if self.unserved(chain.chain_id) => {
                NetworkAvailability::Unavailable(NetworkUnavailable::NoBridge)
            }
            availability => availability,
        }
    }

    /// Whether the routes fetched for `network` and the form's sell token show that no provider
    /// serves it. Routes that aren't fetched yet, or couldn't be, don't.
    pub(super) fn unserved(&self, network: u64) -> bool {
        self.form.as_ref().is_some_and(|form| {
            if let Some(public) = &form.public {
                return public
                    .routes
                    .get(&(form.sell, network))
                    .is_some_and(Vec::is_empty);
            }
            matches!(
                form.bridge.routes.get(&(form.sell, network)),
                Some(Ok(routes)) if !routes.served
            )
        })
    }

    /// Start loading the wallet's session of the chains Private balance could deliver on that
    /// aren't loaded, or of `only` that one of them. A chain that is loading or loaded is left
    /// as it is.
    pub(super) fn load_private_networks(&self, only: Option<u64>, cx: &mut Context<'_, Self>) {
        let own = self.origin_chain_id;
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let chains = root
            .read(cx)
            .effective_chain_configs
            .values()
            .filter(|chain| {
                chain.chain_id != own
                    && only.is_none_or(|only| only == chain.chain_id)
                    && chain.bridge_profile().is_some()
                    && chain.swap_profile().is_some()
                    && resolve_effective_chain_rpc_route(chain.chain_id, chain).is_ok()
            })
            .map(|chain| chain.chain_id)
            .collect::<Vec<_>>();
        if chains.is_empty() {
            return;
        }
        root.update(cx, |root, cx| {
            for chain_id in chains {
                root.ensure_chain_load(chain_id, cx);
            }
        });
    }

    /// The wallet's private totals on `network`, by token.
    pub(super) fn private_totals(
        &self,
        form: &SwapForm,
        network: u64,
        cx: &App,
    ) -> Vec<(Address, U256)> {
        if network == self.origin_chain_id && form.public.is_none() {
            return form.assets.totals.clone();
        }
        self.root.upgrade().map_or_else(Vec::new, |root| {
            root.read(cx)
                .private_action_asset_options(DeliveryFormKind::Unshield, network)
                .into_iter()
                .map(|asset| (asset.token, asset.total))
                .collect()
        })
    }
}
