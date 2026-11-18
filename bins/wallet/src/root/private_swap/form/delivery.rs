use super::*;

impl PrivateSwapsView {
    /// The delivery the form's choices and receiver give. Private balance on another network
    /// needs that network to take it. The receiver parses as Private Unshield's recipients do,
    /// and mustn't be one of the swap's own addresses. A new swap has no stealth account to
    /// compare until it's reserved; signing checks it then. On another network, the receiver is
    /// checked against that network's contracts, and the Buy token needs a provider.
    pub(super) fn form_delivery(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        if form.public.is_some() {
            return self.public_form_delivery(form, cx);
        }
        if form.receive_to == ReceiveTo::PrivateBalance {
            // A non-EVM network has no private balance.
            if let Some(network) = form.non_evm {
                return Err(DeliveryProblem::Network {
                    problem: NetworkAvailability::Unavailable(NetworkUnavailable::PublicOnly)
                        .private_problem(network.display_name())
                        .unwrap_or_default()
                        .into(),
                    syncing: false,
                });
            }
            return match form.network {
                None => Ok(SwapDelivery::Reshield),
                Some(network) => self.private_bridge_delivery(form, network, cx),
            };
        }
        let receiver_problem = |problem: &'static str| DeliveryProblem::Receiver(problem.into());
        let entered = form.receiver_value.trim();
        if entered.is_empty() {
            return Err(receiver_problem(ENTER_RECEIVER));
        }
        // A non-EVM receiver is in its network's own format, which NEAR Intents pays.
        if let Some(network) = form.non_evm {
            let receiver = network.parse_receiver(entered).map_err(|error| {
                DeliveryProblem::Receiver(non_evm_receiver_message(network, error).into())
            })?;
            let BridgeState::NonEvm(destination) = form.bridge_state() else {
                return Err(DeliveryProblem::Bridge);
            };
            return Ok(SwapDelivery::NonEvmBridge(non_evm_delivery(
                network,
                receiver,
                destination,
            )));
        }
        let receiver = parse_address(entered).ok_or_else(|| receiver_problem(INVALID_RECEIVER))?;
        if let Some(network) = form.network {
            return self.bridge_delivery(form, network, receiver, cx);
        }
        let unavailable =
            || receiver_problem("Public address delivery isn't available on this chain.");
        let root = self.root.upgrade().ok_or_else(unavailable)?;
        let chain = root
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)
            .ok_or_else(unavailable)?;
        let profile = chain.swap_profile().ok_or_else(unavailable)?;
        let railgun = chain
            .require_railgun()
            .map_err(|_| unavailable())?
            .deployment
            .contract;
        // The zero address is rejected first, so it stands in for an unknown executor.
        let executor = form
            .operation
            .and_then(|operation| self.record(operation))
            .and_then(ExecutorRecord::address)
            .unwrap_or(Address::ZERO);
        profile
            .check_receiver(railgun, executor, receiver)
            .map_err(|rejection| receiver_problem(receiver_rejection_message(rejection)))?;
        Ok(SwapDelivery::External { receiver })
    }

    /// A Bridge delivery that shields to the wallet on `network`, which Across alone can do. A
    /// swap that still chooses its accounts needs the network to meet Private balance's
    /// conditions: all of them while it sets up a stealth account there, and all but the
    /// setup's funding for an existing account, which takes no setup. A started swap met them
    /// when it was approved. The receiver is the destination stealth account: the chosen
    /// existing one, or for a new one `Address::ZERO`, which stands in until it is reserved, as
    /// no quote names it. A started swap's approval or last order names the account it
    /// reserved.
    fn private_bridge_delivery(
        &self,
        form: &SwapForm,
        network: u64,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        let name = network_name(network);
        let chosen = form.destination_choice();
        if form.chooses_accounts() {
            let availability = self
                .network_availability(ReceiveTo::PrivateBalance, network, cx)
                .unwrap_or(NetworkAvailability::Unavailable(
                    NetworkUnavailable::PublicOnly,
                ));
            let met = if self.destination_setup_chain(form).is_some() {
                availability == NetworkAvailability::Available
            } else {
                availability.admits_existing_account()
            };
            if !met && let Some(problem) = availability.private_problem(&name) {
                return Err(DeliveryProblem::Network {
                    problem: problem.into(),
                    syncing: matches!(
                        availability,
                        NetworkAvailability::Syncing(_)
                            | NetworkAvailability::CheckingFee
                            | NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee)
                    ),
                });
            }
        }
        let BridgeState::Ready { destination, .. } = form.bridge_state() else {
            return Err(DeliveryProblem::Bridge);
        };
        if let Some(account) = chosen.filter(|_| form.chooses_accounts())
            && let Some(problem) =
                self.destination_account_problem(form, account, destination.destination_token, cx)
        {
            return Err(DeliveryProblem::Account(problem.into()));
        }
        let receiver = match chosen {
            Some(account) => account.address,
            None => self
                .reserved_delivery(form)
                .filter(|delivery| delivery.destination_chain == network)
                .map_or(Address::ZERO, |delivery| delivery.receiver),
        };
        Ok(SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: network,
            receiver,
            destination_token: destination.destination_token,
            surplus: bridge_surplus(BridgeProvider::Across, form.bridge.surplus),
            private: Some(form.private_delivery()),
        }))
    }

    /// Why recorded outcomes refuse `account` as the destination of a swap that delivers
    /// `token`. Local records answer this; preparation checks the account's current state, its
    /// balance of `token` and its earlier shields before anything is signed. The draft's own
    /// claim on the account, left by an earlier attempt, isn't a refusal.
    fn destination_account_problem(
        &self,
        form: &SwapForm,
        account: DestinationAccount,
        token: Address,
        cx: &App,
    ) -> Option<String> {
        if self.destination_operation(form) == Some(account.operation) {
            return None;
        }
        let (_, owner) = self.destination_owner(account.chain_id, cx)?;
        let reason = match owner
            .swap_account_refusal(account.operation, SwapAccountRole::Destination { token })
        {
            Ok(None) => return None,
            Ok(Some(refusal)) => refusal.to_string(),
            Err(error) => format!("{error:#}"),
        };
        Some(format!(
            "#{} on {} can't take this swap: {reason}. Choose another account.",
            account.index,
            network_name(account.chain_id)
        ))
    }

    /// The form's reserved account and, for a reused source, its draft's swap-use identity.
    /// A new source is read through the record's own swap even when the form has a use identity.
    pub(super) fn reserved_swap(
        &self,
        form: &SwapForm,
    ) -> Option<(&ExecutorRecord, Option<SwapUseId>)> {
        let record = self.record(form.operation?)?;
        let swap_use = if form.reuse_account {
            Some(form.reuse_use?)
        } else {
            None
        };
        Some((record, swap_use))
    }

    /// The private Bridge delivery the form's swap names once its accounts are reserved: a
    /// started swap's own, or the one the draft's swap use claimed a reused account with.
    pub(super) fn reserved_delivery(&self, form: &SwapForm) -> Option<BridgeDelivery> {
        let (record, swap_use) = self.reserved_swap(form)?;
        if let Some(swap_use) = swap_use {
            return crate::root::private_swap::model::swap_use_destination(record, swap_use)
                .map(|(delivery, _)| delivery);
        }
        swap_private_delivery(record)
    }

    /// A Bridge delivery to `receiver` on `network`, which mustn't be one of that network's
    /// protocol contracts, with the provider the Buy token uses.
    fn bridge_delivery(
        &self,
        form: &SwapForm,
        network: u64,
        receiver: Address,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        let name = network_name(network);
        let unavailable =
            || DeliveryProblem::Receiver(format!("Bridging to {name} isn't available.").into());
        let root = self.root.upgrade().ok_or_else(unavailable)?;
        let chain = root
            .read(cx)
            .effective_chain_configs
            .get(network)
            .ok_or_else(unavailable)?;
        // A chain without a Railgun deployment has no Railgun contract to reject.
        let railgun = chain
            .railgun
            .as_ref()
            .map(|railgun| railgun.deployment.contract);
        chain
            .bridge_destination()
            .ok_or_else(unavailable)?
            .check_receiver(railgun, receiver)
            .map_err(|rejection| {
                DeliveryProblem::Receiver(bridge_receiver_message(rejection, &name).into())
            })?;
        let BridgeState::Ready {
            destination,
            provider,
            ..
        } = form.bridge_state()
        else {
            return Err(DeliveryProblem::Bridge);
        };
        Ok(SwapDelivery::Bridge(BridgeDelivery {
            provider,
            destination_chain: network,
            receiver,
            destination_token: destination.destination_token,
            surplus: bridge_surplus(provider, form.bridge.surplus),
            private: None,
        }))
    }

    /// The receiver suggestions, the same as Private Unshield's: the selected wallet's active
    /// public accounts, then the public address book. On a non-EVM network, that network's
    /// non-EVM address-book entries alone.
    pub(super) fn receiver_options(&self, cx: &App) -> Vec<RecipientOption> {
        let non_evm = self.form.as_ref().and_then(|form| form.non_evm);
        self.root.upgrade().map_or_else(Vec::new, |root| {
            let root = root.read(cx);
            match non_evm {
                Some(network) => non_evm_recipient_options(&root.non_evm_address_book, network),
                None => root.private_unshield_recipient_options(),
            }
        })
    }

    /// The Receiver field's placeholder follows the destination network's address format.
    pub(in crate::root::private_swap) fn sync_receiver_placeholder(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let placeholder = receiver_placeholder(form.non_evm);
        form.receiver_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx);
        });
    }

    /// The receiver's address rules: Private Unshield's, or the non-EVM network's format.
    pub(super) fn receiver_rules(&self) -> RecipientRules {
        self.form
            .as_ref()
            .and_then(|form| form.non_evm)
            .map_or(RecipientRules::Form(RECEIVER_RULES), RecipientRules::NonEvm)
    }

    /// The wallet's label for a non-EVM `receiver` on `network`: its saved entry there.
    pub(in crate::root::private_swap) fn non_evm_receiver_label(
        &self,
        network: NonEvmNetwork,
        receiver: &str,
        cx: &App,
    ) -> Option<String> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        root.non_evm_address_book
            .iter()
            .find(|entry| entry.network == network && entry.address == receiver)
            .map(|entry| entry.label.clone())
    }

    /// The wallet's name for `receiver`: one of its Public accounts, marked `true`, or a public
    /// address-book entry.
    pub(in crate::root::private_swap) fn receiver_label(
        &self,
        receiver: Address,
        cx: &App,
    ) -> Option<(String, bool)> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        if let Some(account) = root
            .public_accounts
            .iter()
            .find(|account| account.address == receiver)
        {
            let label = public_account_display_label(account)
                .unwrap_or_else(|| railgun_ui::short_address(&account.address));
            return Some((label, true));
        }
        root.public_address_book
            .iter()
            .find(|entry| entry.address == receiver)
            .map(|entry| (entry.label.clone(), false))
    }
}
