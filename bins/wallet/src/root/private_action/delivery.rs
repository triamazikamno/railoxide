use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SelfBroadcastPrivacyIconPlacement {
    DeliverySelector,
    PublicFunding,
}

pub(in crate::root) const fn self_broadcast_privacy_icon_placement(
    sponsorship_enabled: bool,
) -> SelfBroadcastPrivacyIconPlacement {
    if sponsorship_enabled {
        SelfBroadcastPrivacyIconPlacement::PublicFunding
    } else {
        SelfBroadcastPrivacyIconPlacement::DeliverySelector
    }
}

pub(in crate::root) fn render_delivery_selector(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    mode: DeliveryMode,
    generating: bool,
    self_broadcast_available: bool,
    sponsorship_enabled: bool,
) -> gpui::Div {
    use ui::private_action::self_broadcast::{DeliveryChoice, delivery_selector};
    delivery_selector(
        delivery_element_id(key, kind, "mode-toggle"),
        match mode {
            DeliveryMode::PublicBroadcaster => DeliveryChoice::Broadcaster,
            DeliveryMode::SelfBroadcast => DeliveryChoice::SelfBroadcast,
            DeliveryMode::ManualCalldata => DeliveryChoice::ExternalWallet,
        },
        self_broadcast_available,
        true,
        self_broadcast_privacy_icon_placement(sponsorship_enabled)
            == SelfBroadcastPrivacyIconPlacement::DeliverySelector,
        generating,
        move |choice, window, cx| {
            let mode = match choice {
                DeliveryChoice::Broadcaster => DeliveryMode::PublicBroadcaster,
                DeliveryChoice::SelfBroadcast => DeliveryMode::SelfBroadcast,
                DeliveryChoice::ExternalWallet => DeliveryMode::ManualCalldata,
            };
            root.update(cx, |root, cx| match kind {
                DeliveryFormKind::Send => root.set_send_delivery_mode(key, mode, window, cx),
                DeliveryFormKind::Unshield => {
                    root.set_unshield_delivery_mode(key, mode, window, cx);
                }
            });
        },
    )
}

pub(in crate::root) fn render_public_broadcaster_settings(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    allow_suspicious_broadcasters: bool,
    favorites_only_broadcasters: bool,
    action_token: Address,
    fee_mode: FeeHandlingMode,
    choice: &BroadcasterChoice,
    candidates: &[PublicBroadcasterCandidate],
    fee_token_options: &[PublicBroadcasterFeeTokenOption],
    selected_fee_token: Address,
    generating: bool,
) -> gpui::Div {
    use ui::private_action::{BroadcasterSettings, BroadcasterSettingsEvent};
    let fee_token = render_fee_token_selector(
        root.clone(),
        key,
        kind,
        fee_token_options,
        selected_fee_token,
        generating,
    );
    let fee_mode = (kind == DeliveryFormKind::Send
        && should_show_fee_mode_toggle(kind, action_token, selected_fee_token))
    .then(|| {
        render_fee_mode_toggle(
            root.clone(),
            key,
            kind,
            DeliveryMode::PublicBroadcaster,
            fee_mode,
            generating,
        )
        .into_any_element()
    });
    ui::private_action::broadcaster_settings(
        delivery_element_id(key, kind, "broadcaster-settings"),
        BroadcasterSettings {
            allow_out_of_range: allow_suspicious_broadcasters,
            favorites_only: favorites_only_broadcasters,
            random_selected: matches!(choice, BroadcasterChoice::Random),
            specific_label: selected_broadcaster_label(choice, candidates),
            candidate_count: candidates.len(),
            disabled: generating,
        },
        fee_token,
        fee_mode,
        move |event, window, cx| {
            root.update(cx, |root, cx| match event {
                BroadcasterSettingsEvent::AllowOutOfRange(checked) => {
                    root.set_allow_suspicious_broadcasters(kind, key, checked, cx);
                }
                BroadcasterSettingsEvent::FavoritesOnly(checked) => {
                    root.set_favorites_only_broadcasters(kind, key, checked, cx);
                }
                BroadcasterSettingsEvent::ChooseSpecific => {
                    root.open_broadcaster_picker(kind, key, window, cx);
                }
                BroadcasterSettingsEvent::Random => match kind {
                    DeliveryFormKind::Send => {
                        root.set_send_broadcaster_choice(key, BroadcasterChoice::Random, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.set_unshield_broadcaster_choice(key, BroadcasterChoice::Random, cx);
                    }
                },
            });
        },
    )
}

pub(in crate::root) fn render_self_broadcast_settings(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    accounts: &[PublicAccountMetadata],
    selected_uuid: Option<&str>,
    balance_snapshot: Option<&PublicBalanceSnapshot>,
    gas_payer_select: &Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>>,
    gas_fee: &Eip1559GasFeeEditorState,
    funding: SelfBroadcastFundingMode,
    incentive: SponsoredIncentive,
    custom_incentive_input: &Entity<InputState>,
    show_sponsored_funding_choice: bool,
    sponsorship_unavailable_reason: Option<&'static str>,
    generating: bool,
    submit_enabled: bool,
    submit: impl Fn(&mut Window, &mut App) + Clone + 'static,
) -> gpui::Div {
    use ui::private_action::self_broadcast::{
        self as shared, FundingChoice, IncentiveChoice, SettingsEvent,
    };
    let funding = if show_sponsored_funding_choice {
        funding
    } else {
        SelfBroadcastFundingMode::PublicBalance
    };
    let sponsored = funding == SelfBroadcastFundingMode::PrivateSponsorship;
    let missing = !accounts.is_empty()
        && !accounts
            .iter()
            .any(|account| Some(account.public_account_uuid.as_str()) == selected_uuid);
    let zero = !sponsored
        && selected_uuid.is_some_and(|uuid| {
            self_broadcast_native_balance_state(balance_snapshot, key.chain_id, uuid)
                == SelfBroadcastNativeBalanceState::Zero
        });
    let random_enabled = accounts.iter().any(|account| {
        if sponsored {
            Some(account.public_account_uuid.as_str()) != selected_uuid
        } else {
            self_broadcast_gas_payer_random_candidate(
                account,
                selected_uuid,
                key.chain_id,
                balance_snapshot,
            )
        }
    });
    let custom_input = custom_incentive_input.clone();
    let gas_editor = crate::root::ui_helpers::input_enter_scope(submit_enabled, submit.clone())
        .child(render_eip1559_gas_fee_editor(
            root.clone(),
            &Eip1559GasFeeTarget::Private { key, kind },
            gas_fee,
            generating,
        ));
    shared::settings(
        delivery_element_id(key, kind, "self-broadcast-settings"),
        shared::Settings {
            funding: if sponsored {
                FundingChoice::Sponsorship
            } else {
                FundingChoice::PublicBalance
            },
            incentive: match incentive {
                SponsoredIncentive::Economy => IncentiveChoice::Economy,
                SponsoredIncentive::Standard => IncentiveChoice::Standard,
                SponsoredIncentive::Priority => IncentiveChoice::Priority,
                SponsoredIncentive::Custom(_) => IncentiveChoice::Custom,
            },
            show_sponsorship: show_sponsored_funding_choice,
            sponsorship_unavailable: sponsorship_unavailable_reason.map(str::to_owned),
            no_signers: accounts.is_empty(),
            signer_error: zero.then(|| SELF_BROADCAST_ZERO_GAS_PAYER_WARNING.to_owned()),
            incentive_error: None,
            random_enabled,
            disabled: generating,
        },
        shared::signer_select(
            gas_payer_select,
            sponsored,
            missing,
            zero,
            generating || accounts.is_empty(),
        ),
        crate::root::ui_helpers::input_enter_scope(submit_enabled, submit).child(
            private_action_input(custom_incentive_input)
                .disabled(generating)
                .w_40(),
        ),
        gas_editor,
        move |event, window, cx| {
            root.update(cx, |root, cx| match event {
                SettingsEvent::Funding(funding) => root.set_self_broadcast_funding_mode(
                    kind,
                    key,
                    if funding == FundingChoice::Sponsorship {
                        SelfBroadcastFundingMode::PrivateSponsorship
                    } else {
                        SelfBroadcastFundingMode::PublicBalance
                    },
                    cx,
                ),
                SettingsEvent::Incentive(incentive) => {
                    let selected = match incentive {
                        IncentiveChoice::Economy => SponsoredIncentive::Economy,
                        IncentiveChoice::Standard => SponsoredIncentive::Standard,
                        IncentiveChoice::Priority => SponsoredIncentive::Priority,
                        IncentiveChoice::Custom => {
                            let Ok(percent) = custom_input.read(cx).value().trim().parse::<u8>()
                            else {
                                return;
                            };
                            SponsoredIncentive::Custom(percent)
                        }
                    };
                    root.set_sponsored_incentive(kind, key, selected, cx);
                }
                SettingsEvent::RandomSigner => {
                    root.choose_random_self_broadcast_gas_payer(kind, key, window, cx);
                }
            });
        },
    )
}

pub(in crate::root) fn render_sponsored_funding_estimate(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    display: &SponsoredFundingEstimateDisplay,
    breakdown_open: bool,
) -> gpui::Div {
    ui::private_action::self_broadcast::estimated_fees(
        delivery_element_id(key, kind, "funding-estimate"),
        display,
        public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS),
        breakdown_open,
        move |open, _window, cx| {
            root.update(cx, |root, cx| {
                root.set_sponsored_funding_breakdown_open(kind, key, open, cx);
            });
        },
    )
}

pub(in crate::root) fn self_broadcast_gas_payer_select_trigger_row(
    label: &str,
    address: &Address,
) -> gpui::Div {
    ui::private_action::self_broadcast::signer_trigger_row(label.to_owned(), short_address(address))
}

pub(in crate::root) fn self_broadcast_gas_payer_select_menu_row(
    label: &str,
    address: &Address,
    chain_id: u64,
    balance: &str,
) -> gpui::Div {
    ui::private_action::self_broadcast::signer_menu_row(
        label.to_owned(),
        short_address(address),
        format!("{balance} {}", native_token_display_label(chain_id)),
    )
}

pub(in crate::root) fn render_unshield_output_toggle(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    chain_id: u64,
    unwrap: bool,
    generating: bool,
) -> gpui::Div {
    let Some((native_label, wrapped_label)) = native_wrapped_output_labels(chain_id) else {
        return div();
    };
    ui::private_action::unshield_output_toggle(
        unshield_element_id(key, "output-toggle"),
        native_label,
        wrapped_label,
        unwrap,
        generating,
        move |unwrap, _, cx| {
            root.update(cx, |root, cx| root.set_unshield_unwrap(key, unwrap, cx));
        },
    )
}

#[cfg(test)]
mod gas_payer_layout_tests {
    use super::*;

    #[gpui::test]
    fn gas_payer_balances_align_at_menu_edge_and_rows_remain_selectable(
        cx: &mut gpui::TestAppContext,
    ) {
        let items = [("A", "1"), ("Longer account", "123.456")]
            .into_iter()
            .map(|(label, balance)| SelfBroadcastGasPayerSelectItem {
                public_account_uuid: Arc::from(label),
                label: Arc::from(label),
                address: Address::ZERO,
                chain_id: 1,
                balance_label: Arc::from(balance),
            })
            .collect();
        crate::root::ui_helpers::select_layout_test::assert_balances_align_and_rows_select(
            cx,
            items,
            [380.0, 320.0],
            false,
            ["gas-payer-balance-A", "gas-payer-balance-Longer account"],
            "Longer account",
        );
    }
}
