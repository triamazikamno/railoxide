use super::*;

impl PrivateSwapsView {
    pub(super) fn swap_source_items(
        &self,
        sell: Address,
        cx: &App,
    ) -> Vec<SelectGroup<SwapSourceSelectItem>> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let chain = root.effective_chain_configs.get(self.origin_chain_id);
        let private_available =
            self.session.is_some() && chain.and_then(EffectiveChainConfig::swap_profile).is_some();
        let private_balance = root
            .private_action_asset_options(DeliveryFormKind::Unshield, self.origin_chain_id)
            .into_iter()
            .find(|asset| asset.token == sell)
            .map_or(U256::ZERO, |asset| asset.max_batched);
        let private = SwapSourceSelectItem {
            account: None,
            label: "Private balance".into(),
            balance: self.token_amount(sell, private_balance, cx).into(),
            address: None,
            unavailable: !private_available,
            reason: (!private_available).then(|| "Private swaps unavailable here".into()),
            heading: false,
        };
        let wallet = root
            .view_session
            .as_ref()
            .map(|session| session.wallet_id());
        let public = root
            .public_accounts
            .iter()
            .filter(|account| wallet.is_some_and(|wallet| account.is_active_for_wallet(wallet)))
            .filter(|account| !matches!(account.source, PublicAccountSource::ExecutorDerived(_)))
            .map(|account| {
                // An account whose balances are read holds none of a token they don't list.
                let balance = crate::root::public_balances::public_account_balances_for_chain(
                    root.public_balance_snapshot.as_deref(),
                    self.origin_chain_id,
                    &account.public_account_uuid,
                    account.status,
                )
                .and_then(|balances| {
                    let entry = balances
                        .iter()
                        .find(|entry| entry.asset.id.token_address().unwrap_or_default() == sell);
                    let Some(entry) = entry else {
                        return Some(format!("0 {}", self.token_symbol(sell, cx)));
                    };
                    // A balance the read failed for stays unavailable.
                    entry.amount.amount().map(|_| {
                        format!(
                            "{} {}",
                            public_balance_amount_label(&entry.amount, entry.asset.decimals),
                            entry.asset.symbol
                        )
                    })
                })
                .unwrap_or_else(|| "Balance unavailable".to_owned());
                // A network without a SpokePool says why once, under Pay from.
                let bridges = chain
                    .and_then(EffectiveChainConfig::bridge_origin_profile)
                    .is_some();
                SwapSourceSelectItem {
                    account: Some(account.public_account_uuid.clone()),
                    label: public_source_label(account).into(),
                    address: Some(account.address),
                    balance: balance.into(),
                    unavailable: !bridges || account.is_global(),
                    reason: (bridges && account.is_global()).then(|| SHARED_ACCOUNT_REASON.into()),
                    heading: false,
                }
            })
            .collect::<Vec<_>>();
        let heading = SwapSourceSelectItem::heading(format!(
            "Public accounts on {}",
            network_name(self.origin_chain_id)
        ));
        vec![
            SelectGroup::new("")
                .item(private)
                .item(heading)
                .items(public),
        ]
    }

    /// The Pay from row, with a tooltip explaining why a chosen stealth account fixes the payer.
    pub(super) fn render_pay_from(form: &SwapForm, editable: bool) -> gpui::Div {
        // A chosen stealth account spends the private balance, so the payer can't change.
        let hint = (editable && form.public.is_none() && form.operation.is_some()).then(|| {
            if form.account_select.is_some() {
                "To pay from another account, set Stealth account to New account."
            } else {
                "This swap keeps its stealth account, so its payer can't be changed."
            }
        });
        let editable = editable && form.can_change_source();
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(app_muted_text("Pay from").flex_none())
            .child(
                div()
                    .id("swap-pay-from")
                    .flex_1()
                    .min_w_0()
                    .debug_selector(|| "swap-pay-from".into())
                    .when_some(hint, |this, hint| {
                        this.tooltip(move |window, cx| Tooltip::new(hint).build(window, cx))
                    })
                    .child(
                        Select::new(&form.pay_from_select)
                            .w_full()
                            .disabled(!editable),
                    ),
            )
    }

    pub(super) fn refresh_source_select(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let items = self.swap_source_items(form.sell, cx);
        let value = form
            .public
            .as_ref()
            .map(|public| public.account.public_account_uuid.clone());
        form.pay_from_select.update(cx, |select, cx| {
            select.set_items(SwapSourceSelect::new(items), window, cx);
            select.set_selected_value(&value, window, cx);
        });
    }

    /// Changing the payer creates a fresh draft and drops every reviewed bound and in-flight
    /// request. Only the user's compatible token, amount and destination choices carry over.
    pub(super) fn select_form_source(
        &mut self,
        source: Option<&str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // The control commits a choice before it reports it, so one rejected below would
        // stay shown. Put back the payer the form has; an accepted choice builds a new form.
        let current = form
            .public
            .as_ref()
            .map(|public| public.account.public_account_uuid.clone());
        if form.pay_from_select.read(cx).selected_value() != Some(&current) {
            form.pay_from_select.update(cx, |select, cx| {
                select.set_selected_value(&current, window, cx);
            });
        }
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        if !form.can_change_source()
            || form
                .public
                .as_ref()
                .map(|public| public.account.public_account_uuid.as_str())
                == source
        {
            return;
        }
        let account = if let Some(uuid) = source {
            let Some(root) = self.root.upgrade() else {
                return;
            };
            let root = root.read(cx);
            let Some(account) = root
                .public_accounts
                .iter()
                .find(|account| account.public_account_uuid == uuid)
                .filter(|account| {
                    !account.is_global()
                        && !matches!(account.source, PublicAccountSource::ExecutorDerived(_))
                })
                .filter(|account| {
                    root.view_session
                        .as_ref()
                        .is_some_and(|view| account.is_active_for_wallet(view.wallet_id()))
                })
                .filter(|_| {
                    root.effective_chain_configs
                        .get(self.origin_chain_id)
                        .and_then(EffectiveChainConfig::bridge_origin_profile)
                        .is_some()
                })
            else {
                return;
            };
            Some(account.clone())
        } else {
            if self.session.is_none() || self.swap_profile(cx).is_none() {
                return;
            }
            None
        };
        let old_sell = form.sell;
        let sell_assets = match &account {
            Some(account) => self.public_form_assets(account, old_sell, cx).sell_assets,
            None => self.sell_assets(None, cx),
        };
        let sell = sell_assets
            .iter()
            .find(|asset| asset.token == old_sell && !asset.total.is_zero())
            .or_else(|| sell_assets.first())
            .map_or(old_sell, |asset| asset.token);
        let amount = (sell == old_sell)
            .then(|| self.form_amount(form, cx).ok())
            .flatten();
        let buy = form.buy;
        let network = form.network;
        // A Public account's form offers no non-EVM network.
        let non_evm = (form.non_evm, form.non_evm_token.clone());
        let destination_account = form.destination_choice();
        let terms = SavedTerms {
            slippage_bps: Some(form.slippage_bps),
            gas_share_bps: Some(form.gas_share_bps),
            valid_for: Some(form.valid_for),
        };
        let failure = form.bridge.shield_failure;
        let delivery = if account.is_some() {
            SwapDelivery::Reshield
        } else {
            form.delivery
                .as_ref()
                .cloned()
                .unwrap_or(SwapDelivery::Reshield)
        };
        self.open_form_with_source(
            None,
            sell,
            None,
            amount,
            Some(terms),
            delivery,
            account,
            window,
            cx,
        );
        if let Some(form) = self.form.as_mut() {
            form.network = network;
            form.buy = buy;
            if form.public.is_none() {
                (form.non_evm, form.non_evm_token) = non_evm;
                form.picker.network = form.network_key(self.origin_chain_id);
            }
            form.bridge.shield_failure = failure;
            form.destination_account = destination_account;
            form.renew_reuse_use();
            if form.public.is_some() {
                form.receive_to = ReceiveTo::PrivateBalance;
                form.native_output = false;
                form.bridge.chosen = Some(BridgeProvider::Across);
            }
        }
        if let Some(network) = network {
            self.load_private_networks(Some(network), cx);
        }
        self.refresh_destination_accounts(window, cx);
        self.refresh_setup_route(cx);
        self.refresh_form_delivery(cx);
        self.load_bridge_routes(window, cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    pub(super) fn public_form_assets(
        &self,
        account: &PublicAccountMetadata,
        _: Address,
        cx: &App,
    ) -> FormAssets {
        let Some(root) = self.root.upgrade() else {
            return FormAssets {
                sell_assets: Vec::new(),
                totals: Vec::new(),
                locked: U256::ZERO,
                sell_receivable: false,
            };
        };
        let root = root.read(cx);
        let mut assets = crate::root::public_account_visible_balances_for_chain(
            root.public_balance_snapshot.as_deref(),
            self.origin_chain_id,
            &account.public_account_uuid,
            account.status,
        )
        .into_iter()
        .filter_map(|entry| {
            let amount = entry.amount.amount()?;
            if amount.is_zero() {
                return None;
            }
            Some(UnshieldAsset {
                chain_id: self.origin_chain_id,
                token: entry.asset.id.token_address().unwrap_or_default(),
                label: entry.asset.symbol,
                decimals: Some(entry.asset.decimals),
                total: amount,
                poi_verified_total: amount,
                max_batched: amount,
                icon_path: public_asset_icon_path(
                    self.origin_chain_id,
                    entry.asset.id,
                    Some(&root.effective_token_registry),
                ),
            })
        })
        .collect::<Vec<_>>();
        assets.sort_by_key(|asset| (asset.token != Address::ZERO, asset.label.clone()));
        FormAssets {
            totals: assets
                .iter()
                .map(|asset| (asset.token, asset.total))
                .collect(),
            sell_assets: assets,
            locked: U256::ZERO,
            sell_receivable: false,
        }
    }

    pub(in crate::root::private_swap) fn open_new_form(
        &mut self,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        // Debug UI fixture: a `setup:` or `retry-review` mode shows its private swap instead.
        #[cfg(debug_assertions)]
        if crate::root::private_swap::ui_fixture::setup_swap_token(sell).is_some()
            && self.open_ui_fixture_swap(window, cx)
        {
            return;
        }
        self.open_form(
            None,
            sell,
            None,
            None,
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
    }

    /// Explicit account selection keeps normal swaps on their fresh-account path.
    pub(in crate::root) fn open_account_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let records = self.private_owner().and_then(|owner| owner.records().ok());
        let record = records.and_then(|records| {
            records
                .into_iter()
                .find(|record| record.operation() == operation)
        });
        let Some(record) = record.filter(|record| record.address().is_some()) else {
            return;
        };
        let sell = swap_tokens(&record).map(|(sell, _)| sell).or_else(|| {
            self.sell_assets(Some(operation), cx)
                .first()
                .map(|asset| asset.token)
        });
        let Some(sell) = sell else {
            return;
        };
        self.open_form(
            None,
            sell,
            None,
            None,
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
        // Offer this account even when it can't swap now; its check explains why.
        let items = self.swap_account_items(Some(&record), cx);
        if let Some(select) = self
            .form
            .as_ref()
            .and_then(|form| form.account_select.clone())
        {
            select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(items), window, cx);
            });
        }
        self.select_form_account(Some(operation), window, cx);
    }

    /// The accounts a new swap can use as its own: a new one first, then set-up accounts,
    /// newest first. `chosen` is listed even when the local rules leave it out.
    pub(super) fn swap_account_items(
        &self,
        chosen: Option<&ExecutorRecord>,
        cx: &App,
    ) -> Vec<SwapAccountSelectItem> {
        // Debug UI fixture: its synthesized records stand in for the store's, under the same
        // local admission the store's candidates pass.
        let mut accounts = if crate::root::private_swap::ui_fixture::holds_swaps() {
            self.records
                .iter()
                .filter(|record| {
                    !record.is_hidden()
                        && swap_account_refusal(
                            record,
                            self.origin_chain_id,
                            SwapAccountRole::Source,
                            SwapAccountUse::New,
                            SwapAdmissionEvidence::Recorded,
                        )
                        .is_none()
                })
                .filter_map(|record| {
                    Some((
                        record.operation(),
                        record.index(),
                        record.address()?,
                        record
                            .swap()
                            .map(|swap| (swap.terms().sell_token(), swap.terms().buy_token())),
                        record.purpose_intent().map(str::to_owned),
                    ))
                })
                .collect::<Vec<_>>()
        } else {
            self.private_owner()
                .and_then(|owner| owner.swap_account_candidates().ok())
                .unwrap_or_default()
                .into_iter()
                .map(|candidate| {
                    (
                        candidate.operation(),
                        candidate.index(),
                        candidate.address(),
                        candidate.last_pair(),
                        candidate.purpose().map(str::to_owned),
                    )
                })
                .collect::<Vec<_>>()
        };
        if let Some(record) = chosen
            && let Some(address) = record.address()
            && !accounts
                .iter()
                .any(|(operation, ..)| *operation == record.operation())
        {
            accounts.push((
                record.operation(),
                record.index(),
                address,
                record
                    .swap()
                    .map(|swap| (swap.terms().sell_token(), swap.terms().buy_token())),
                record.purpose_intent().map(str::to_owned),
            ));
        }
        self.account_items(accounts, cx)
    }

    /// The accounts a private Bridge swap can deliver to on the form's network: a new one
    /// first, then that network's set-up accounts, newest first. The form's choice is listed
    /// even when the local rules leave it out. Their earlier pairs name tokens of that
    /// network, so they aren't shown.
    fn destination_account_items(&self, form: &SwapForm, cx: &App) -> Vec<SwapAccountSelectItem> {
        let mut accounts = form
            .network
            .and_then(|network| form.destination_candidates.get(&network))
            .into_iter()
            .flatten()
            .map(|candidate| {
                (
                    candidate.operation(),
                    candidate.index(),
                    candidate.address(),
                    None,
                    candidate.purpose().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        if let Some(chosen) = form.destination_choice()
            && !accounts
                .iter()
                .any(|(operation, ..)| *operation == chosen.operation)
        {
            accounts.push((chosen.operation, chosen.index, chosen.address, None, None));
        }
        self.account_items(accounts, cx)
    }

    /// A new account, then `accounts`, newest first.
    fn account_items(&self, mut accounts: Vec<AccountRow>, cx: &App) -> Vec<SwapAccountSelectItem> {
        accounts.sort_by_key(|(_, index, ..)| std::cmp::Reverse(*index));
        let mut items = vec![new_account_item()];
        items.extend(accounts.into_iter().map(
            |(operation, index, address, last_pair, purpose)| {
                let mut label = vec![format!("#{index}"), railgun_ui::short_address(&address)];
                if let Some((sell, buy)) = last_pair {
                    label.push(format!(
                        "Last used {} → {}",
                        self.token_symbol(sell, cx),
                        self.token_symbol(buy, cx)
                    ));
                } else if let Some(purpose) = purpose {
                    label.push(purpose);
                }
                SwapAccountSelectItem {
                    operation: Some(operation),
                    address: Some(address),
                    label: label.join(" · ").into(),
                }
            },
        ));
        items
    }

    /// Use a set-up stealth account for this new swap, or go back to a new account. The
    /// tokens, amount and slippage stay, and the swap is quoted again using recorded account
    /// state. Execution preparation checks the chosen account before proving.
    pub(super) fn select_form_account(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(select) = form.account_select.clone() else {
            return;
        };
        if !busy && form.operation != operation {
            if let Some(operation) = operation {
                self.tracking.entry(operation).or_default().auto_place = false;
            }
            form.operation = operation;
            form.reuse_account = operation.is_some();
            form.renew_reuse_use();
            form.error = None;
            // Each account quotes on its own orderbook route, which keeps the accounts
            // unlinked there. The old account's route lookups are dropped too.
            form.set_orderbook(None, None);
            form.bridge.routes_tasks.clear();
            form.bridge.non_evm_tasks.clear();
            form.route.invalidate_estimate();
            form.destination_route.invalidate_estimate();
        }
        let current = form.operation;
        if select.read(cx).selected_value() != Some(&current) {
            select.update(cx, |select, cx| {
                select.set_selected_value(&current, window, cx);
            });
        }
        // Load the chosen account's record, which may not belong to a swap, and drop a
        // previous one.
        self.reload_records();
        // The account decides which notes the swap can spend.
        self.refresh_form_assets(cx);
        // A receiver can't be the swap's own stealth account.
        self.refresh_form_delivery(cx);
        // Debug UI fixture: its form sets nothing up, so no setup fee is estimated for it.
        if !crate::root::private_swap::ui_fixture::holds_swaps() {
            self.refresh_setup_route(cx);
        }
        self.load_bridge_routes(window, cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Deliver a private Bridge swap to a set-up stealth account on the form's network, or go
    /// back to a new account there. The swap's own account, the tokens and the amount stay.
    /// The swap is quoted again, which clears the acknowledgements, and the destination's
    /// setup route follows the choice. Preparation checks the chosen account before signing.
    pub(super) fn select_form_destination(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(select) = form.destination_select.clone() else {
            return;
        };
        let current = form.destination_choice();
        // Only an account of the form's own network can be chosen: the listed ones, or the
        // one already chosen.
        let account = operation
            .zip(form.network.filter(|_| form.private_bridge()))
            .and_then(|(operation, chain_id)| {
                current
                    .filter(|account| account.operation == operation)
                    .or_else(|| {
                        form.destination_candidates
                            .get(&chain_id)?
                            .iter()
                            .find(|candidate| candidate.operation() == operation)
                            .map(|candidate| DestinationAccount {
                                chain_id,
                                operation,
                                index: candidate.index(),
                                address: candidate.address(),
                            })
                    })
            });
        if !busy && current != account {
            form.destination_account = account;
            form.renew_reuse_use();
            form.error = None;
            form.destination_route.invalidate_estimate();
        }
        let chosen = form.destination_choice().map(|account| account.operation);
        if select.read(cx).selected_value() != Some(&chosen) {
            select.update(cx, |select, cx| {
                select.set_selected_value(&chosen, window, cx);
            });
        }
        self.refresh_setup_route(cx);
        // The delivery names the chosen account, and its check says why it can't be used.
        self.delivery_changed(window, cx);
    }

    /// Read again, from local records, the set-up accounts each loaded network offers as a
    /// private Bridge swap's destination, and list the form's network's in the destination
    /// select. Nothing is asked of a network. A network without setup funds can be picked
    /// while it has such an account.
    pub(in crate::root::private_swap) fn refresh_destination_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref().filter(|form| {
            form.receive_to == ReceiveTo::PrivateBalance
                && form.chooses_accounts()
                && (form.picker.open || form.network.is_some())
        }) else {
            return;
        };
        let own = self.origin_chain_id;
        // Recorded outcomes decide who is listed; the token is checked when the swap is
        // prepared.
        let token = form.buy.unwrap_or_default();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let chains = root
            .read(cx)
            .effective_chain_configs
            .values()
            .filter(|chain| chain.chain_id != own && chain.bridge_profile().is_some())
            .map(|chain| chain.chain_id)
            .filter(|chain_id| form.picker.open || form.network == Some(*chain_id))
            .collect::<Vec<_>>();
        let candidates = chains
            .into_iter()
            .filter_map(|chain_id| {
                let (_, owner) = self.destination_owner(chain_id, cx)?;
                Some((
                    chain_id,
                    owner.swap_destination_candidates(token).unwrap_or_default(),
                ))
            })
            .collect::<HashMap<_, _>>();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.destination_candidates != candidates {
            form.destination_candidates = candidates;
            self.refresh_destination_select(window, cx);
            cx.notify();
        }
    }

    /// List the form's network's accounts in the destination select, with the form's choice
    /// selected.
    pub(super) fn refresh_destination_select(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(select) = form.destination_select.clone() else {
            return;
        };
        let items = self.destination_account_items(form, cx);
        let chosen = form.destination_choice().map(|account| account.operation);
        select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            select.set_selected_value(&chosen, window, cx);
        });
    }

    /// Whether `chain_id` has a set-up account that local records admit as a private Bridge
    /// swap's destination.
    pub(super) fn destination_reusable(&self, chain_id: u64) -> bool {
        self.form.as_ref().is_some_and(|form| {
            form.destination_candidates
                .get(&chain_id)
                .is_some_and(|candidates| !candidates.is_empty())
        })
    }
}
