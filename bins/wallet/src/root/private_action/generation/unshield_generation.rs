use super::*;

mod authorized_generation;

impl WalletRoot {
    pub(in crate::root) fn generate_unshield_calldata_from_form(
        &mut self,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.confirm_private_input_pin_admission(DeliveryFormKind::Unshield, key, window, cx) {
            return;
        }
        let Some(draft) = self.unshield_spend_draft(key, cx) else {
            return;
        };
        // Checked before the form's operation is replaced or cleared below.
        if self.unshield_submission_abandons_executor_operation(key, &draft) {
            Self::confirm_double_payment_risk(window, cx, move |root, window, cx| {
                if let Some(form) = root.unshield_forms.get_mut(&key) {
                    form.executor_operation = None;
                    form.executor_review = None;
                }
                root.generate_unshield_calldata_from_form(key, window, cx);
            });
            return;
        }
        if Self::unshield_requires_executor(&draft) {
            if draft.delivery_mode == DeliveryMode::ManualCalldata {
                self.set_unshield_form_error(key, "External-wallet export is unavailable for this stealth-account action. Select a supported delivery method.", cx);
                return;
            }
            if let Err(reason) = self.unshield_draft_executor_route(&draft) {
                self.set_unshield_form_error(key, reason.to_string(), cx);
                return;
            }
            self.request_executor_unshield_authorization(key, &draft, window, cx);
            return;
        }
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.executor_review = None;
            form.executor_operation = None;
        }
        let fee_review = match broadcaster_fee_review(
            draft.delivery_mode,
            draft.custom_fee_amount,
            draft.cost_estimate.as_ref(),
        ) {
            Ok(review) => review,
            Err(error) => {
                self.set_unshield_form_error(key, error.to_string(), cx);
                return;
            }
        };
        let requires_gas_payer_password = self.selected_wallet_source().is_hardware_derived()
            && self_broadcast_requires_software_gas_payer_password(
                draft.delivery_mode,
                draft.self_broadcast_gas_payer_source,
            );
        let intent = if requires_gas_payer_password {
            SpendAuthorizationIntent::PrivateUnshieldSelfBroadcastGasPassword(
                key,
                draft.sponsored_authorization_limit,
                self.unshield_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
            )
        } else {
            SpendAuthorizationIntent::PrivateUnshield(
                key,
                draft.sponsored_authorization_limit,
                self.unshield_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
                draft
                    .custom_fee_amount
                    .map(|amount| (draft.fee_token, amount)),
                fee_review.clone(),
            )
        };
        let summary = if requires_gas_payer_password {
            private_unshield_gas_payer_authorization_summary(&draft)
        } else {
            private_unshield_authorization_summary(&draft)
        };
        let summary = if let Some(amount) = draft.custom_fee_amount {
            summary.with_custom_transaction_fee(
                crate::root::private_action::fee_editor::custom_fee_label(
                    draft.asset.chain_id,
                    draft.fee_token,
                    amount,
                    &self.effective_token_registry,
                ),
            )
        } else if let Some(review) = &fee_review {
            with_broadcaster_fee_rows(
                summary,
                draft.asset.chain_id,
                review,
                draft.fee_mode,
                &self.effective_token_registry,
            )
        } else {
            summary
        };
        let summary = if let Some(disclosure) = draft
            .cost_estimate
            .as_ref()
            .filter(|_| draft.delivery_mode == DeliveryMode::PublicBroadcaster)
            .and_then(|estimate| unbundling_disclosure(estimate, &draft.asset.label))
        {
            summary.with_notice(AppNoticeTone::Warning, disclosure)
        } else {
            summary
        }
        .with_privacy_hints(self.unshield_privacy_presentation(key));
        self.request_spend_authorization(intent, summary, window, cx);
    }

    pub(in crate::root) fn unshield_spend_draft(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.unshield_spend_draft_with_quote_admission(key, true, cx)
    }

    pub(in crate::root) fn unshield_spend_draft_for_prepared_review(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.unshield_spend_draft_with_quote_admission(key, false, cx)
    }

    fn unshield_spend_draft_with_quote_admission(
        &mut self,
        key: UnshieldAssetKey,
        require_current_quote: bool,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.refresh_unshield_native_top_up_state(key, cx);
        let delivery_mode = {
            let form = self.unshield_forms.get(&key)?;
            if form.generating {
                return None;
            }
            form.delivery_mode
        };
        self.ensure_waku_for_delivery(delivery_mode, cx);
        let form = self.unshield_forms.get(&key)?;
        let self_broadcast = delivery_mode == DeliveryMode::SelfBroadcast;
        let estimate_is_current = if self_broadcast {
            matches!(
                form.sponsored_funding_estimate,
                Some(
                    SponsoredFundingEstimateState::Ready(_)
                        | SponsoredFundingEstimateState::PublicBalanceReady(_)
                )
            ) && self.gateway_self_broadcast_review_current(form.gateway_execution.as_ref(), cx)
        } else {
            broadcaster_estimate_allows_submission(
                form.cost_estimate.as_ref(),
                form.cost_estimate_pending || form.estimating_cost,
            )
        };
        if require_current_quote
            // A prepared executor review pays its own approved quote and bounds.
            && (((form.custom_fee_amount.is_some()
                || (delivery_mode == DeliveryMode::PublicBroadcaster
                    && form.executor_review.is_none()))
                && !estimate_is_current)
                || !gateway_private_estimate_is_ready(
                    form.gateway_execution.as_ref(),
                    estimate_is_current,
                    form.gateway_estimated_at,
                    !(self_broadcast
                        && form.self_broadcast_funding
                            == SelfBroadcastFundingMode::PrivateSponsorship),
                ))
        {
            self.set_unshield_form_error(
                key,
                "Refresh the current fee estimate before submitting this review.",
                cx,
            );
            return None;
        }
        let asset = form.asset.clone();
        let unwrap = form.unwrap;
        let recipient_input = form.recipient_input.clone();
        let amount_input = form.amount_input.clone();
        let broadcaster_choice = form.broadcaster_choice.clone();
        let cost_estimate = form.cost_estimate.clone();
        let fee_token = form.selected_fee_token;
        let custom_fee_amount = form.custom_fee_amount;
        let self_broadcast_funding = effective_delivery_funding_mode(
            delivery_mode,
            self.effective_chain_configs.get(asset.chain_id),
            form.self_broadcast_funding,
        );
        let sponsored_incentive = if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
        {
            match sponsored_incentive_from_text(
                form.sponsored_incentive,
                form.sponsored_custom_incentive_input
                    .read(cx)
                    .value()
                    .as_ref(),
            ) {
                Ok(incentive) => incentive,
                Err(error) => {
                    self.set_unshield_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            form.sponsored_incentive
        };
        let self_broadcast_gas_payer_uuid = form.self_broadcast_gas_payer_uuid.clone();
        let self_broadcast_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            match form.self_broadcast_gas_fee.selection(cx) {
                Ok(selection) => selection,
                Err(error) => {
                    self.set_unshield_form_error(key, error, cx);
                    return None;
                }
            }
        } else {
            SelfBroadcastGasFeeSelection::Auto
        };
        let self_broadcast_initial_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            self_broadcast_initial_gas_values(
                &self_broadcast_gas_fee,
                form.self_broadcast_gas_fee.quote,
            )
        } else {
            None
        };
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            asset.token,
            fee_token,
            form.fee_mode,
        );
        let allow_suspicious_broadcasters = form.allow_suspicious_broadcasters;
        let favorites_only_broadcasters = form.favorites_only_broadcasters;
        if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
            && let Some(reason) = sponsored_self_broadcast_availability_reason(
                self.effective_chain_configs.get(asset.chain_id),
            )
        {
            self.set_unshield_form_error(key, reason, cx);
            return None;
        }

        let Some(view_session) = self.view_session.clone() else {
            self.set_unshield_form_error(key, "Unlock the wallet vault before unshielding", cx);
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.set_unshield_form_error(key, "Wallet vault storage is unavailable", cx);
            return None;
        };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            self.set_unshield_form_error(
                key,
                "You can prepare this form now, but generation is available after wallet sync finishes",
                cx,
            );
            return None;
        };
        let session = Arc::clone(session);
        if asset.max_batched.is_zero() {
            self.set_unshield_form_error(
                key,
                "No POI-verified private notes are spendable in a batched unshield",
                cx,
            );
            return None;
        }

        let recipient_raw = recipient_input.read(cx).value().to_string();
        let Some(recipient) = parse_address(recipient_raw.trim()) else {
            self.set_unshield_form_error(key, "Enter a valid public EVM recipient address", cx);
            return None;
        };
        let amount_raw = amount_input.read(cx).value().to_string();
        let amount = match parse_unshield_amount(amount_raw.as_str(), asset.decimals) {
            Ok(amount) if !amount.is_zero() => amount,
            Ok(_) => {
                self.set_unshield_form_error(key, "Enter an amount greater than zero", cx);
                return None;
            }
            Err(error) => {
                self.set_unshield_form_error(key, error.to_string(), cx);
                return None;
            }
        };
        let max_entered_amount = unshield_form_max_entered_amount(form, delivery_mode, fee_mode)
            .unwrap_or(asset.max_batched);
        if amount > max_entered_amount {
            self.set_unshield_form_error(
                key,
                format!(
                    "Amount exceeds max POI-verified batched transaction: {}",
                    railgun_ui::group_thousands(&format_unshield_amount_input(
                        max_entered_amount,
                        asset.decimals
                    ))
                ),
                cx,
            );
            return None;
        }
        let native_top_up =
            enabled_native_top_up_plan(form.native_top_up_enabled, form.native_top_up.as_ref());

        let (
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            self_broadcast_gas_payer_address,
        ) = if delivery_mode == DeliveryMode::SelfBroadcast {
            let Some(uuid) = self_broadcast_gas_payer_uuid else {
                self.set_unshield_form_error(key, "Choose a Public transaction signer", cx);
                return None;
            };
            let Some(account) = self.selected_self_broadcast_gas_payer_account(Some(uuid.as_ref()))
            else {
                self.set_unshield_form_error(key, "Choose an active Public transaction signer", cx);
                return None;
            };
            let gas_payer_display = public_account_display_label(account).map_or_else(
                || short_address(&account.address),
                |label| format!("{label} · {}", short_address(&account.address)),
            );
            (
                Some(uuid.to_string()),
                Some(gas_payer_display),
                Some(account.source),
                Some(account.address),
            )
        } else {
            (None, None, None, None)
        };
        let signer_native_balance_snapshot =
            self_broadcast_public_account_uuid
                .as_deref()
                .map_or(U256::ZERO, |uuid| {
                    self_broadcast_native_balance_amount(
                        self.public_balance_snapshot.as_deref(),
                        asset.chain_id,
                        uuid,
                    )
                });

        let sponsored_authorization_limit = if self_broadcast_funding
            == SelfBroadcastFundingMode::PrivateSponsorship
        {
            let Some(effective_chain) = self.effective_chain_configs.get(asset.chain_id) else {
                self.set_unshield_form_error(key, "Selected chain settings are unavailable", cx);
                return None;
            };
            let Some((max_fee_per_gas, max_priority_fee_per_gas)) = self_broadcast_initial_gas_fee
            else {
                self.set_unshield_form_error(
                    key,
                    "Refresh self-broadcast gas fees before authorizing sponsorship",
                    cx,
                );
                return None;
            };
            match quote_sponsored_unshield_authorization_limit(
                asset.chain_id,
                effective_chain,
                &session.unspent_utxos(),
                asset.token,
                amount,
                fee_mode,
                recipient,
                unwrap,
                native_top_up.as_ref(),
                max_fee_per_gas,
                max_priority_fee_per_gas,
                signer_native_balance_snapshot,
                sponsored_incentive,
                self_broadcast_gas_payer_address.expect("self-broadcast signer was validated"),
            ) {
                Ok(limit) => Some(limit),
                Err(error) => {
                    self.set_unshield_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            None
        };
        let sponsored_authorization_display = sponsored_authorization_limit.map(|limit| {
            sponsored_authorization_display(
                asset.chain_id,
                limit,
                &self.effective_token_registry,
                &self.public_broadcaster_anchor_cache,
            )
        });

        // No live-advertisement check: delivery needs a current estimate, the review pins that
        // quote, and wallet-ops re-applies the trust filter and fee policy to it before proving.
        let fee_rows = if delivery_mode == DeliveryMode::PublicBroadcaster {
            self.monitor_fee_rows()
        } else {
            Vec::new()
        };
        let fee_policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);

        Some(UnshieldSpendDraft {
            custom_fee_amount,
            executor_review: self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.executor_review.clone()),
            asset,
            unwrap,
            delivery_mode,
            broadcaster_choice,
            cost_estimate,
            fee_token,
            self_broadcast_gas_fee,
            self_broadcast_funding,
            sponsored_incentive,
            sponsored_authorization_limit,
            sponsored_authorization_display,
            self_broadcast_initial_gas_fee,
            fee_mode,
            view_session,
            vault_store,
            session,
            recipient,
            amount,
            native_top_up,
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            fee_rows,
            fee_policy,
            favorites_only_broadcasters,
        })
    }
}
