use super::*;

impl PrivateSwapsView {
    pub(super) fn token_decimals(&self, token: Address, cx: &App) -> Option<u8> {
        if token == Address::ZERO {
            return self
                .root
                .upgrade()?
                .read(cx)
                .effective_chain_configs
                .get(self.origin_chain_id)
                .map(|chain| chain.native_currency.decimals);
        }
        self.token_metadata(token, cx)
            .map(|metadata| metadata.decimals)
    }

    /// The chain's wrapped native token, which a Public address can receive as the native asset.
    pub(super) fn wrapped_native_token(&self, cx: &App) -> Option<Address> {
        let root = self.root.upgrade()?;
        root.read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .wrapped_native_token
    }

    /// Whether the Buy panel offers native output: a Public address receives the chain's
    /// wrapped native token.
    pub(super) fn offers_native_output(&self, form: &SwapForm, cx: &App) -> bool {
        form.receive_to == ReceiveTo::PublicAddress
            && form.network.is_none()
            && form.buy.is_some()
            && form.buy == self.wrapped_native_token(cx)
    }

    pub(super) fn form_amount(&self, form: &SwapForm, cx: &App) -> Result<U256, String> {
        let amount = wallet_ops::parse_unshield_amount(
            &form.amount_input.read(cx).value(),
            self.form_sell_metadata(form, cx)
                .map(|metadata| metadata.decimals),
        )
        .map_err(|error| format!("{error:#}"))?;
        if amount.is_zero() {
            return Err("Enter an amount to swap.".into());
        }
        Ok(amount)
    }

    pub(super) fn form_sell_metadata(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Option<crate::root::tokens::TokenDisplayMetadata> {
        if form.public.is_some()
            && let Some(asset) = form
                .assets
                .sell_assets
                .iter()
                .find(|asset| asset.token == form.sell)
            && let Some(decimals) = asset.decimals
        {
            return Some(crate::root::tokens::TokenDisplayMetadata {
                symbol: asset.label.clone(),
                decimals,
                icon_path: asset.icon_path.clone(),
            });
        }
        self.token_metadata(form.sell, cx)
    }

    pub(super) fn form_sell_amount(&self, form: &SwapForm, amount: U256, cx: &App) -> String {
        if form.public.is_some()
            && let Some(metadata) = self.form_sell_metadata(form, cx)
        {
            return format!(
                "{} {}",
                railgun_ui::format_token_amount(amount, metadata.decimals),
                metadata.symbol
            );
        }
        self.token_amount(form.sell, amount, cx)
    }

    pub(super) fn form_inputs_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    pub(super) fn set_form_sell(
        &mut self,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.form.as_ref().is_none_or(|form| {
            (form.operation.is_some() && !form.reuse_account) || form.sell == token
        }) {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.sell = token;
        form.error = None;
        if form.network.is_none() && form.buy == Some(token) {
            form.buy = None;
            form.native_output = false;
        }
        form.route.invalidate_estimate();
        if form.public.is_some() {
            form.destination_route.invalidate_estimate();
            self.refresh_form_assets(cx);
            self.refresh_source_select(window, cx);
            self.load_bridge_routes(window, cx);
            self.refresh_setup_route(cx);
            self.refresh_form_delivery(cx);
            self.schedule_quote(window, cx);
            cx.notify();
            return;
        }
        self.refresh_form_assets(cx);
        self.refresh_source_select(window, cx);
        // A Bridge swap's routes depend on the sell token.
        self.load_bridge_routes(window, cx);
        self.refresh_provider_select(window, cx);
        self.refresh_setup_route(cx);
        self.refresh_form_delivery(cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    pub(super) fn set_form_buy(
        &mut self,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if (form.operation.is_some() && !form.reuse_account) || form.buy == Some(token) {
            return;
        }
        form.buy = Some(token);
        form.native_output = false;
        form.error = None;
        if form.network.is_some() {
            // The token decides the provider, unless the user picked one.
            self.refresh_provider_select(window, cx);
            self.refresh_form_delivery(cx);
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    pub(super) fn set_form_provider(
        &mut self,
        provider: BridgeProvider,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.network.is_none()
            || (form.operation.is_some() && !form.reuse_account)
            || form.bridge.chosen == Some(provider)
        {
            return;
        }
        let before = form.bridge_terms();
        form.bridge.chosen = Some(provider);
        form.error = None;
        if form.bridge_terms() == before {
            // Picking the provider already in use changes nothing to quote.
            self.refresh_provider_select(window, cx);
            cx.notify();
            return;
        }
        self.bridge_choices_changed(window, cx);
    }

    pub(super) fn set_form_surplus(
        &mut self,
        surplus: BridgeSurplus,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.bridge.surplus == surplus || self.is_retry(form) {
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.bridge.surplus = surplus;
        }
        self.delivery_changed(window, cx);
    }

    /// Choose what a private Bridge delivery does when its shield on the destination network
    /// can't run. The choice is part of the delivery, so the swap is quoted again, which clears
    /// the acknowledgements.
    pub(super) fn set_form_shield_failure(
        &mut self,
        choice: BridgeShieldFailure,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.bridge.shield_failure == choice || self.is_retry(form) {
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.bridge.shield_failure = choice;
        }
        self.delivery_changed(window, cx);
    }

    /// Pay the wrapped native Buy asset to the Public address as the native asset, or as the
    /// wrapped token. The order's buy token changes, so the swap is quoted again.
    pub(super) fn set_native_output(
        &mut self,
        native: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if (form.operation.is_some() && !form.reuse_account)
            || form.native_output == native
            || (native && !self.offers_native_output(form, cx))
        {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.native_output = native;
        form.error = None;
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Choose where the bought token goes, from the form's Receive to or the Buy picker's
    /// switch. Only a Public address can receive the native asset, so any change returns the
    /// output to the wrapped token. The network and the Buy asset stay: when the network can't
    /// take the new delivery kind, [`Self::form_delivery`] says why, and nothing is quoted.
    pub(super) fn set_receive_to(
        &mut self,
        receive_to: ReceiveTo,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.receive_to_locked(form) || form.receive_to == receive_to {
            return;
        }
        // Across pays a Public address the wrapped native token as the native asset, and
        // shields it as the wrapped token. The Buy asset is the same route either way.
        let unwrapped = self
            .destination_chain(form, cx)
            .and_then(|chain| across_unwrapped_token(&chain));
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.receive_to = receive_to;
        form.blocked_shield_delivery_changed();
        form.native_output = false;
        form.receiver_picker.set((false, None));
        if let Some(wrapped) = unwrapped {
            match (receive_to, form.buy) {
                (ReceiveTo::PrivateBalance, Some(Address::ZERO)) => form.buy = Some(wrapped),
                (ReceiveTo::PublicAddress, Some(buy)) if buy == wrapped => {
                    form.buy = Some(Address::ZERO);
                }
                _ => {}
            }
        }
        let (network, bridged) = (form.network, form.bridged());
        // Private balance on the kept network needs the wallet's session there. A non-EVM
        // network has none, and the form says why it can't take Private balance.
        if receive_to == ReceiveTo::PrivateBalance && network.is_some() {
            self.load_private_networks(network, cx);
        }
        self.settle_buy_picker(window, cx);
        if bridged {
            self.bridge_choices_changed(window, cx);
        } else {
            self.delivery_changed(window, cx);
        }
    }

    /// The receiver input changed: search the suggestions, then check and quote the entry.
    pub(super) fn receiver_edited(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let (rules, options) = (self.receiver_rules(), self.receiver_options(cx));
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let value = form.receiver_input.read(cx).value().to_string();
        if form.receiver_value == value {
            return;
        }
        form.receiver_value = value;
        let suggestions = recipient_suggestions_for_input(rules, &options, &form.receiver_value);
        form.receiver_picker.set(suggestions);
        self.receiver_changed(window, cx);
    }

    /// Use `receiver`, as a picked suggestion does.
    fn set_form_receiver(
        &mut self,
        receiver: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.receiver_picker.set((false, None));
        if form.receiver_value == receiver {
            cx.notify();
            return;
        }
        receiver.clone_into(&mut form.receiver_value);
        form.receiver_input.update(cx, |input, cx| {
            input.set_value(receiver.to_owned(), window, cx);
        });
        // Programmatic input changes don't emit InputEvent::Change.
        self.receiver_changed(window, cx);
    }

    pub(super) fn receiver_picker_event(
        &mut self,
        event: &RecipientPickerEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let RecipientPickerEvent::Select(receiver) = event {
            self.set_form_receiver(receiver, window, cx);
            return;
        }
        let (rules, options) = (self.receiver_rules(), self.receiver_options(cx));
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form
            .receiver_picker
            .apply(rules, &options, &form.receiver_value, event)
        {
            cx.notify();
        }
    }

    /// Enter in the receiver input picks the highlighted suggestion.
    pub(super) fn confirm_receiver_suggestion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .form
            .as_ref()
            .filter(|form| form.receiver_picker.open())
        else {
            return;
        };
        let Some(receiver) = recipient_suggestion_to_confirm(
            self.receiver_rules(),
            &self.receiver_options(cx),
            &form.receiver_value,
            form.receiver_picker.index(),
        ) else {
            return;
        };
        self.set_form_receiver(&receiver, window, cx);
    }

    /// The delivery choice changed: check it, then quote again, which clears the
    /// acknowledgements.
    pub(super) fn delivery_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            self.refresh_form_delivery(cx);
            self.schedule_quote(window, cx);
            cx.notify();
            return;
        }
        self.refresh_form_delivery(cx);
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// The receiver changed: check it, and clear the acknowledgements. No quote names the
    /// receiver, so a quote of the form's terms stays. A ready one takes the receiver here,
    /// without a request, and one in flight takes it when it lands. Without such a quote, the
    /// swap is quoted.
    fn receiver_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.refresh_form_delivery(cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // A non-EVM review can't take another receiver without I/O, so it is quoted again.
        let quoted = form.non_evm.is_none()
            && form.quote_terms.is_some()
            && form.quote_terms == self.form_quote_terms(form, cx)
            && !matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_));
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.error = None;
        if !quoted {
            self.schedule_quote(window, cx);
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        form.continuing_acknowledged = false;
        let moved = match &form.quote {
            QuoteState::Ready(review) => {
                let moved = review.with_receiver(form.quote_receiver());
                (moved.plan().delivery() != review.plan().delivery()).then(|| Arc::new(moved))
            }
            _ => None,
        };
        if let Some(review) = moved {
            // A bridge refresh that is due or under way was asked for the replaced review, and
            // its reply would be dropped. Ask again for this one.
            let refresh = review.bridge().is_some()
                && form.gas_share_pending(&review)
                && form.bridge_quote_error.is_none();
            form.quote = QuoteState::Ready(review);
            if refresh {
                self.schedule_bridge_quote(window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn refresh_form_delivery(&mut self, cx: &App) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let delivery = self.form_delivery(form, cx);
        if let Some(form) = self.form.as_mut() {
            form.delivery = delivery;
        }
    }

    pub(super) fn use_amount(
        &mut self,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref().filter(|form| form.speed_up.is_none()) else {
            return;
        };
        let value = format_unshield_amount_input(
            amount,
            self.form_sell_metadata(form, cx)
                .map(|metadata| metadata.decimals),
        );
        let input = form.amount_input.clone();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        // Programmatic input changes don't emit InputEvent::Change.
        self.form_inputs_changed(window, cx);
    }

    /// Swap the sell and buy tokens. The amount belonged to the old sell token, so it clears.
    pub(super) fn flip_tokens(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (sell, Some(buy)) = (form.sell, form.buy) else {
            return;
        };
        let (input, sell_select) = (form.amount_input.clone(), form.sell_select.clone());
        input.update(cx, |input, cx| input.set_value("", window, cx));
        sell_select.update(cx, |select, cx| {
            select.set_selected_value(&buy, window, cx);
        });
        self.set_form_sell(buy, window, cx);
        self.set_form_buy(sell, window, cx);
    }
}
