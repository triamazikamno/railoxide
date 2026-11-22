use super::*;

impl WalletRoot {
    pub(in crate::root) fn set_self_broadcast_funding_mode(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        funding: SelfBroadcastFundingMode,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    form.self_broadcast_funding = funding;
                    form.error = None;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    form.self_broadcast_funding = funding;
                    form.error = None;
                    form.result = None;
                }
                self.refresh_unshield_self_broadcast_plan(key, cx);
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn set_sponsored_incentive(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        incentive: SponsoredIncentive,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    form.sponsored_incentive = incentive;
                    form.error = None;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    form.sponsored_incentive = incentive;
                    form.error = None;
                    form.result = None;
                }
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn set_sponsored_custom_incentive_from_text(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        value: &str,
        cx: &mut Context<'_, Self>,
    ) {
        let parsed = value
            .trim()
            .parse::<u8>()
            .ok()
            .map(SponsoredIncentive::Custom)
            .filter(|incentive| incentive.percent().is_ok());
        let error = parsed.is_none().then(|| {
            Arc::<str>::from("Custom builder incentive must be an integer from 1% through 100%.")
        });
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    if let Some(incentive) = parsed {
                        form.sponsored_incentive = incentive;
                    }
                    form.error = error;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    if let Some(incentive) = parsed {
                        form.sponsored_incentive = incentive;
                    }
                    form.error = error;
                    form.result = None;
                }
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn active_self_broadcast_gas_payer_accounts(
        &self,
    ) -> Vec<PublicAccountMetadata> {
        self.public_accounts
            .iter()
            .filter(|account| {
                account.status == PublicAccountStatus::Active
                    && account.is_available_on_chain(self.last_evm_chain)
            })
            .cloned()
            .collect()
    }

    pub(in crate::root) fn default_self_broadcast_gas_payer_uuid(&self) -> Option<Arc<str>> {
        default_self_broadcast_gas_payer_uuid(&self.active_self_broadcast_gas_payer_accounts())
    }

    pub(in crate::root) fn new_self_broadcast_gas_payer_select(
        &self,
        chain_id: u64,
        selected_uuid: Option<&str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>> {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let items = self_broadcast_gas_payer_select_items(
            &accounts,
            chain_id,
            self.public_balance_snapshot.as_deref(),
            &cx.weak_entity(),
        );
        let selected_index = self_broadcast_gas_payer_select_index(&items, selected_uuid);
        cx.new(|cx| {
            SelectState::new(FullWidthSelectItems::new(items), selected_index, window, cx)
                .searchable(true)
        })
    }

    pub(in crate::root) fn new_private_action_asset_select(
        &self,
        kind: DeliveryFormKind,
        chain_id: u64,
        selected_token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> (
        Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
        Vec<PrivateActionAssetSelectItem>,
    ) {
        let assets = self.private_action_asset_options(kind, chain_id);
        let items = private_action_asset_select_items(&assets);
        let selected_index = private_action_asset_select_index(&items, selected_token);
        let state_items = items.clone();
        (
            cx.new(|cx| {
                SelectState::new(SearchableVec::new(items), selected_index, window, cx)
                    .searchable(true)
            }),
            state_items,
        )
    }

    pub(in crate::root) fn sync_self_broadcast_gas_payer_selects(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let snapshot = self.public_balance_snapshot.clone();
        let mut changed = Vec::new();
        for (key, form) in &mut self.send_forms {
            let selected = normalized_self_broadcast_gas_payer_uuid(
                form.self_broadcast_gas_payer_uuid.as_ref(),
                &accounts,
            );
            if form.self_broadcast_gas_payer_uuid != selected {
                changed.push((DeliveryFormKind::Send, *key));
            }
            form.self_broadcast_gas_payer_uuid.clone_from(&selected);
            sync_self_broadcast_gas_payer_select_entity(
                &form.self_broadcast_gas_payer_select,
                &accounts,
                form.asset.chain_id,
                snapshot.as_deref(),
                selected.as_ref(),
                window,
                cx,
            );
        }
        for (key, form) in &mut self.unshield_forms {
            let selected = normalized_self_broadcast_gas_payer_uuid(
                form.self_broadcast_gas_payer_uuid.as_ref(),
                &accounts,
            );
            if form.self_broadcast_gas_payer_uuid != selected {
                changed.push((DeliveryFormKind::Unshield, *key));
            }
            form.self_broadcast_gas_payer_uuid.clone_from(&selected);
            sync_self_broadcast_gas_payer_select_entity(
                &form.self_broadcast_gas_payer_select,
                &accounts,
                form.asset.chain_id,
                snapshot.as_deref(),
                selected.as_ref(),
                window,
                cx,
            );
        }
        for (kind, key) in changed {
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn revalidate_sponsored_estimates_for_public_balance_change(
        &mut self,
        previous: Option<&PublicBalanceSnapshot>,
        current: &PublicBalanceSnapshot,
        cx: &mut Context<'_, Self>,
    ) {
        let mut changed = Vec::new();
        for (key, form) in &self.send_forms {
            if !form.generating
                && form.asset.chain_id == current.chain_id
                && form.delivery_mode == DeliveryMode::SelfBroadcast
                && form.self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
                && form
                    .self_broadcast_gas_payer_uuid
                    .as_deref()
                    .is_some_and(|uuid| {
                        sponsored_signer_balance_snapshot_changed(
                            previous,
                            current,
                            current.chain_id,
                            uuid,
                        )
                    })
            {
                changed.push((DeliveryFormKind::Send, *key));
            }
        }
        for (key, form) in &self.unshield_forms {
            if !form.generating
                && form.asset.chain_id == current.chain_id
                && form.delivery_mode == DeliveryMode::SelfBroadcast
                && form.self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
                && form
                    .self_broadcast_gas_payer_uuid
                    .as_deref()
                    .is_some_and(|uuid| {
                        sponsored_signer_balance_snapshot_changed(
                            previous,
                            current,
                            current.chain_id,
                            uuid,
                        )
                    })
            {
                changed.push((DeliveryFormKind::Unshield, *key));
            }
        }
        for (kind, key) in changed {
            self.revalidate_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn sync_self_broadcast_gas_payer_select(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let snapshot = self.public_balance_snapshot.clone();
        match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get_mut(&key) else {
                    return;
                };
                sync_self_broadcast_gas_payer_select_entity(
                    &form.self_broadcast_gas_payer_select,
                    &accounts,
                    form.asset.chain_id,
                    snapshot.as_deref(),
                    form.self_broadcast_gas_payer_uuid.as_ref(),
                    window,
                    cx,
                );
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get_mut(&key) else {
                    return;
                };
                sync_self_broadcast_gas_payer_select_entity(
                    &form.self_broadcast_gas_payer_select,
                    &accounts,
                    form.asset.chain_id,
                    snapshot.as_deref(),
                    form.self_broadcast_gas_payer_uuid.as_ref(),
                    window,
                    cx,
                );
            }
        }
    }

    pub(in crate::root) fn selected_self_broadcast_gas_payer_account(
        &self,
        selected_uuid: Option<&str>,
    ) -> Option<&PublicAccountMetadata> {
        let selected_uuid = selected_uuid?;
        self.public_accounts.iter().find(|account| {
            account.status == PublicAccountStatus::Active
                && account.public_account_uuid == selected_uuid
                && account.is_available_on_chain(self.last_evm_chain)
        })
    }
}
