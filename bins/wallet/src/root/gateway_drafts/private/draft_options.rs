#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root::gateway_drafts) fn private_draft_asset(
        &self,
        input: &GatewayPrivateDraftInput,
    ) -> Result<UnshieldAsset, String> {
        if self
            .view_session
            .as_ref()
            .is_none_or(|wallet| wallet.wallet_id() != input.wallet)
            || self.last_evm_chain != input.chain_id
        {
            return Err("Wallet or network changed. Open a new draft.".into());
        }
        let token = input
            .asset
            .parse::<Address>()
            .map_err(|_| "Choose an available asset")?;
        self.private_action_asset_options(kind(input), input.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)
            .ok_or_else(|| "Choose an available private asset".into())
    }

    pub(in crate::root::gateway_drafts) fn private_draft_recipient(
        &self,
        input: &GatewayPrivateDraftInput,
    ) -> Result<String, String> {
        if input.address_book_entry.is_none() {
            return Ok(input.recipient.clone());
        }
        let wallet = self
            .view_session
            .as_ref()
            .ok_or("Unlock the desktop wallet")?;
        let store = self
            .vault_store
            .as_ref()
            .ok_or("Wallet storage is unavailable")?;
        resolve_private_draft_contact(store, wallet, input)
    }

    pub(super) fn private_draft_inputs(
        &self,
        input: &GatewayPrivateDraftInput,
        recipient: String,
    ) -> Result<PrivateEstimateInput, String> {
        let delivery = input
            .delivery
            .broadcaster()
            .ok_or("Choose broadcaster delivery")?;
        let asset = self.private_draft_asset(input)?;
        let fee_token = delivery
            .fee_token
            .parse::<Address>()
            .map_err(|_| "Choose a fee token")?;
        let fee_mode = match input.fee_mode {
            GatewayPrivateFeeMode::Deduct => FeeHandlingMode::DeductFromAmount,
            GatewayPrivateFeeMode::AddOnTop => FeeHandlingMode::AddToAmount,
        };
        let fee_mode =
            crate::root::effective_fee_handling_mode(kind(input), asset.token, fee_token, fee_mode);
        let output = self.private_draft_output(input, &asset, &recipient, fee_mode)?;
        let amount = if input.max {
            format_send_amount_input(asset.max_batched, asset.decimals)
        } else {
            input.amount.clone()
        };
        Ok(PrivateEstimateInput {
            custom_fee_amount: None,
            asset,
            recipient,
            amount,
            fee_token,
            fee_mode,
            output,
            broadcaster: match &delivery.broadcaster {
                GatewayBroadcasterChoice::Random => BroadcasterChoice::Random,
                GatewayBroadcasterChoice::Specific { id } => BroadcasterChoice::Specific {
                    railgun_address: id.clone(),
                },
            },
            shown_broadcaster: None,
            refused_broadcasters: Vec::new(),
            input_pin: None,
            executor_operation: None,
            allow_out_of_range: delivery.allow_out_of_range,
            favorites_only: delivery.favorites_only,
        })
    }

    pub(in crate::root::gateway_drafts) fn private_draft_output(
        &self,
        input: &GatewayPrivateDraftInput,
        asset: &UnshieldAsset,
        recipient: &str,
        fee_mode: FeeHandlingMode,
    ) -> Result<PrivateEstimateOutput, String> {
        Ok(match input.kind {
            GatewayPrivateDraftKind::PrivateSend => {
                if input.unwrap || input.native_top_up {
                    return Err("Native output options are only available for Unshield.".into());
                }
                PrivateEstimateOutput::Send
            }
            GatewayPrivateDraftKind::Unshield => {
                if input.unwrap
                    && !is_effective_wrapped_native_token(
                        &self.effective_chain_configs,
                        input.chain_id,
                        asset.token,
                    )
                {
                    return Err("This asset cannot be unwrapped.".into());
                }
                if wallet_ops::unshield_requires_executor(
                    input.unwrap,
                    input.native_top_up,
                    gateway_private_sponsored(input),
                ) && let Err(reason) = self.unshield_executor_route(input.chain_id)
                {
                    return Err(reason.to_string());
                }
                let native_top_up = if input.native_top_up {
                    let recipient = recipient
                        .parse::<Address>()
                        .map_err(|_| "Enter a public recipient")?;
                    let amount = if input.max {
                        asset.max_batched
                    } else {
                        wallet_ops::parse_unshield_amount(&input.amount, asset.decimals)
                            .map_err(|error| error.to_string())?
                    };
                    Some(
                        self.unshield_native_top_up_state(
                            input.chain_id,
                            recipient,
                            asset.token,
                            input.unwrap,
                            amount,
                            fee_mode,
                        )
                        .plan
                        .ok_or("Native top-up is unavailable for these inputs or private funds.")?,
                    )
                } else {
                    None
                };
                PrivateEstimateOutput::Unshield {
                    unwrap: input.unwrap,
                    native_top_up,
                }
            }
        })
    }

    pub(in crate::root::gateway_drafts) fn gateway_private_draft_options(
        &self,
        input: &GatewayPrivateDraftInput,
        prepared: Option<&PreparedPrivateDraft>,
        picker: Option<&PrivateDraftPicker>,
    ) -> GatewayPrivateDraftOptions {
        let mut options = GatewayPrivateDraftOptions::default();
        let icon = |source: &Option<crate::assets::WalletIconSource>| match source {
            Some(crate::assets::WalletIconSource::Embedded(path))
                if path.starts_with("railgun-ui/") =>
            {
                Some(path.clone())
            }
            _ => None,
        };
        options.assets = self
            .private_action_asset_options(kind(input), input.chain_id)
            .iter()
            .map(|asset| {
                let mut choice = GatewayPrivateAssetChoice::default();
                choice.id = asset.token.to_string();
                choice.label.clone_from(&asset.label);
                let maximum = if input.delivery.broadcaster().is_none()
                    && input.kind == GatewayPrivateDraftKind::Unshield
                    && input.fee_mode == GatewayPrivateFeeMode::AddOnTop
                {
                    crate::root::unshield_max_entered_amount_for_mode(
                        asset.max_batched,
                        FeeHandlingMode::AddToAmount,
                    )
                } else {
                    asset.max_batched
                };
                choice.max_amount = format_send_amount_input(maximum, asset.decimals);
                choice.max_amount_label =
                    private_action_metric_display_amount(maximum, asset.decimals);
                choice.available =
                    private_action_metric_display_amount(asset.poi_verified_total, asset.decimals);
                choice.icon = icon(&asset.icon_path);
                choice
            })
            .collect();
        options.self_broadcast = Some(self.gateway_self_broadcast_options(input));
        let Ok(asset) = self.private_draft_asset(input) else {
            return options;
        };
        if asset.total > asset.max_batched {
            options.warnings.push(
                ui::private_action::spend_capacity_warning(
                    input.kind == GatewayPrivateDraftKind::Unshield,
                )
                .into(),
            );
        }
        if input.native_top_up {
            options
                .warnings
                .push(ui::private_action::NATIVE_TOP_UP_LINKAGE_WARNING.into());
        }
        let fee_token = match input.delivery.broadcaster() {
            Some(delivery) => match delivery.fee_token.parse::<Address>() {
                Ok(token) => token,
                Err(_) => return options,
            },
            None => asset.token,
        };
        options.show_fee_mode = crate::root::public_broadcaster::should_show_fee_mode_toggle(
            kind(input),
            asset.token,
            fee_token,
        );
        options.show_fee_mode &= input.delivery.broadcaster().is_some()
            || input.kind == GatewayPrivateDraftKind::Unshield;
        if input.kind == GatewayPrivateDraftKind::Unshield {
            // Without an executor route, unwrap and top-up are offered only to switch them off,
            // and the reason is shown whenever either would apply.
            let executor_unavailable = if gateway_private_sponsored(input) {
                None
            } else {
                self.unshield_executor_route(input.chain_id).err()
            };
            let wrapped_native = is_effective_wrapped_native_token(
                &self.effective_chain_configs,
                input.chain_id,
                asset.token,
            );
            if wrapped_native && (input.unwrap || executor_unavailable.is_none()) {
                options.unwrap_labels = crate::root::native_wrapped_output_labels(input.chain_id)
                    .map(|(native, wrapped)| [native.to_owned(), wrapped.to_owned()]);
            }
            let recipient = prepared
                .map_or(input.recipient.as_str(), |prepared| {
                    prepared.input.recipient.as_str()
                })
                .parse::<Address>();
            let amount = if input.max {
                Ok(
                    if input.delivery.broadcaster().is_none()
                        && input.fee_mode == GatewayPrivateFeeMode::AddOnTop
                    {
                        crate::root::unshield_max_entered_amount_for_mode(
                            asset.max_batched,
                            FeeHandlingMode::AddToAmount,
                        )
                    } else {
                        asset.max_batched
                    },
                )
            } else {
                wallet_ops::parse_unshield_amount(&input.amount, asset.decimals)
            };
            let top_up_plan = if let (Ok(recipient), Ok(amount)) = (recipient, amount) {
                let fee_mode = match input.fee_mode {
                    GatewayPrivateFeeMode::Deduct => FeeHandlingMode::DeductFromAmount,
                    GatewayPrivateFeeMode::AddOnTop => FeeHandlingMode::AddToAmount,
                };
                let fee_mode = crate::root::effective_fee_handling_mode(
                    kind(input),
                    asset.token,
                    fee_token,
                    fee_mode,
                );
                self.unshield_native_top_up_state(
                    input.chain_id,
                    recipient,
                    asset.token,
                    input.unwrap,
                    amount,
                    fee_mode,
                )
                .plan
            } else {
                None
            };
            if let Some(reason) = &executor_unavailable
                && (wrapped_native || top_up_plan.is_some())
            {
                options.warnings.push(reason.to_string());
            }
            if let Some(plan) = top_up_plan
                && (input.native_top_up || executor_unavailable.is_none())
            {
                let mut top_up = GatewayPrivateTopUpOption::default();
                top_up.label = format!(
                    "Also send {} for gas",
                    crate::root::native_token_display_label(input.chain_id)
                );
                top_up.funding_detail = format!(
                    "Recipient receives {}; funded from private {}.",
                    crate::root::format_native_token_amount_for_display(
                        input.chain_id,
                        plan.native_amount
                    ),
                    crate::root::format_token_amount_for_display(
                        input.chain_id,
                        plan.wrapped_native_token,
                        plan.wrapped_native_amount,
                        Some(&self.effective_token_registry)
                    )
                );
                options.native_top_up = Some(top_up);
            }
        }
        let Some(delivery) = input.delivery.broadcaster() else {
            return options;
        };
        let policy = self.public_broadcaster_fee_policy(delivery.allow_out_of_range);
        options.fee_tokens = self
            .current_public_broadcaster_fee_token_options(
                input.chain_id,
                input.unwrap,
                input.native_top_up,
                delivery.favorites_only,
                policy,
            )
            .iter()
            .map(|token| {
                let mut choice = GatewayPrivateAssetChoice::default();
                choice.id = token.token.to_string();
                choice.label.clone_from(&token.label);
                choice.max_amount = format_send_amount_input(token.max_spendable, token.decimals);
                choice.max_amount_label =
                    private_action_metric_display_amount(token.max_spendable, token.decimals);
                choice.available.clone_from(&choice.max_amount_label);
                choice.broadcaster_count = token.eligible_broadcaster_count;
                choice.icon = icon(&token.icon_path);
                choice
            })
            .collect();
        let fee_options = self.current_public_broadcaster_fee_token_options(
            input.chain_id,
            input.unwrap,
            input.native_top_up,
            delivery.favorites_only,
            policy,
        );
        if let Some(warning) = crate::root::public_broadcaster_fee_token_warning(
            &self.monitor_fee_rows(),
            input.chain_id,
            &fee_options,
            fee_token,
            &self.public_broadcaster_trust_filter(delivery.favorites_only),
        ) {
            options.warnings.push(warning.into());
        }
        let candidates = self.current_public_broadcaster_candidates(
            input.chain_id,
            fee_token,
            input.unwrap,
            input.native_top_up,
            delivery.favorites_only,
            policy,
        );
        let choice = match &delivery.broadcaster {
            GatewayBroadcasterChoice::Random => BroadcasterChoice::Random,
            GatewayBroadcasterChoice::Specific { id } => BroadcasterChoice::Specific {
                railgun_address: id.clone(),
            },
        };
        if let Some(warning) = crate::root::broadcaster_picker::selected_broadcaster_fee_warning(
            &choice,
            &candidates,
            delivery.allow_out_of_range,
        ) {
            options.warnings.push(warning);
        }
        let candidates = if delivery.allow_out_of_range {
            candidates
        } else {
            wallet_ops::fee_policy_eligible_public_broadcasters(&candidates, policy)
        };
        let candidates = wallet_ops::sort_specific_public_broadcasters(
            candidates,
            &self.public_broadcaster_sort_seed,
        );
        options.candidate_count = candidates.len();
        options.specific_label =
            crate::root::broadcaster_picker::selected_broadcaster_label(&choice, &candidates);
        if let Some(picker) = picker {
            let view_id = &picker.view_id;
            let query = &picker.query;
            let context = prepared.or(picker.estimate.as_ref()).map(|prepared| {
                crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateContext::from(
                    &prepared.estimate,
                )
            });
            let selected = match &delivery.broadcaster {
                GatewayBroadcasterChoice::Specific { id } => Some(id.as_str()),
                GatewayBroadcasterChoice::Random => None,
            };
            let candidates: Vec<_> = candidates
                .into_iter()
                .filter(|candidate| {
                    crate::root::broadcaster_picker::broadcaster_candidate_matches_query(
                        candidate, query,
                    )
                })
                .collect();
            let rows = self.private_broadcaster_picker_rows(
                &candidates,
                policy,
                context.as_ref(),
                selected,
                if picker.job.is_some() {
                    "Estimating…"
                } else if picker.retry_at.is_some() {
                    "Retrying…"
                } else {
                    "Estimate unavailable"
                },
            );
            let mut projection = GatewayPrivateDraftPicker::default();
            projection.view_id.clone_from(view_id);
            projection.query.clone_from(query);
            projection.total_count = options.candidate_count;
            if let Ok(rows) = rows
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
            {
                projection.rows = rows;
                options.picker = Some(projection);
            }
        }
        options
    }
}
