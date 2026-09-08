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
    let selector_root = root;
    let privacy_icon_placement = self_broadcast_privacy_icon_placement(sponsorship_enabled);
    div().flex().flex_col().gap_2().child(
        ButtonGroup::new(delivery_element_id(key, kind, "mode-toggle"))
            .w_full()
            .children(vec![
                private_action_segment_button(
                    delivery_element_id(key, kind, "public"),
                    "Public broadcaster",
                    mode == DeliveryMode::PublicBroadcaster,
                )
                .disabled(generating),
                private_action_segment_button_with_accessory(
                    delivery_element_id(key, kind, "self"),
                    "Self-broadcast",
                    mode == DeliveryMode::SelfBroadcast,
                    (privacy_icon_placement == SelfBroadcastPrivacyIconPlacement::DeliverySelector)
                        .then(|| {
                            render_self_broadcast_privacy_icon(delivery_element_id(
                                key,
                                kind,
                                "self-privacy-warning",
                            ))
                        }),
                )
                .disabled(generating || !self_broadcast_available),
                private_action_segment_button(
                    delivery_element_id(key, kind, "manual"),
                    "External wallet",
                    mode == DeliveryMode::ManualCalldata,
                )
                .disabled(generating),
            ])
            .on_click(move |selected, window, cx| {
                let Some(index) = selected.first() else {
                    return;
                };
                let mode = match *index {
                    0 => DeliveryMode::PublicBroadcaster,
                    1 => DeliveryMode::SelfBroadcast,
                    2 => DeliveryMode::ManualCalldata,
                    _ => return,
                };
                selector_root.update(cx, |root, cx| match kind {
                    DeliveryFormKind::Send => {
                        root.set_send_delivery_mode(key, mode, window, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.set_unshield_delivery_mode(key, mode, window, cx);
                    }
                });
            }),
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
    let fee_token_root = root.clone();
    let fee_mode_root = root.clone();
    let random_root = root.clone();
    let modal_root = root.clone();
    let policy_label_root = root.clone();
    let policy_switch_root = root.clone();
    let favorites_label_root = root.clone();
    let favorites_switch_root = root;
    let specific_label = selected_broadcaster_label(choice, candidates);
    let random_selected = matches!(choice, BroadcasterChoice::Random);
    let specific_selected = matches!(choice, BroadcasterChoice::Specific { .. });
    let selector_disabled = generating || candidates.is_empty();
    let random_button = app_button(
        delivery_element_id(key, kind, "random"),
        "Random broadcaster",
    )
    .flex_1()
    .min_w(px(0.0))
    .selected(random_selected)
    .disabled(selector_disabled);
    let random_button = if random_selected {
        random_button.primary()
    } else {
        random_button
    };
    let specific_button = app_button(
        delivery_element_id(key, kind, "choose-specific"),
        specific_label,
    )
    .flex_1()
    .min_w(px(0.0))
    .selected(specific_selected)
    .disabled(selector_disabled);
    let specific_button = if specific_selected {
        specific_button.primary()
    } else {
        specific_button
    };

    let settings = div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(app_muted_text("Allow out-of-range fees"))
                        .child(cost_estimate_detail_text(
                            "Allow broadcaster fees outside the configured anchor range.",
                        ))
                        .when(!generating, |this| {
                            this.on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                                cx.stop_propagation();
                                policy_label_root.update(cx, |root, cx| {
                                    root.set_allow_suspicious_broadcasters(
                                        kind,
                                        key,
                                        !allow_suspicious_broadcasters,
                                        cx,
                                    );
                                });
                            })
                        }),
                )
                .child(render_danger_switch(
                    delivery_element_id(key, kind, "allow-suspicious-broadcasters"),
                    allow_suspicious_broadcasters,
                    generating,
                    move |checked, _window, cx| {
                        policy_switch_root.update(cx, |root, cx| {
                            root.set_allow_suspicious_broadcasters(kind, key, checked, cx);
                        });
                    },
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(app_muted_text("Favorites only"))
                        .child(cost_estimate_detail_text(
                            "Only use broadcasters saved in your favorites list.",
                        ))
                        .when(!generating, |this| {
                            this.on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                                cx.stop_propagation();
                                favorites_label_root.update(cx, |root, cx| {
                                    root.set_favorites_only_broadcasters(
                                        kind,
                                        key,
                                        !favorites_only_broadcasters,
                                        cx,
                                    );
                                });
                            })
                        }),
                )
                .child(render_switch(
                    delivery_element_id(key, kind, "favorites-only-broadcasters"),
                    favorites_only_broadcasters,
                    generating,
                    theme::PRIMARY,
                    move |checked, _window, cx| {
                        favorites_switch_root.update(cx, |root, cx| {
                            root.set_favorites_only_broadcasters(kind, key, checked, cx);
                        });
                    },
                )),
        )
        .child(render_fee_token_selector(
            fee_token_root,
            key,
            kind,
            fee_token_options,
            selected_fee_token,
            generating,
        ))
        .child(
            ButtonGroup::new(delivery_element_id(key, kind, "broadcaster-choice-toggle"))
                .w_full()
                .disabled(selector_disabled)
                .child(random_button)
                .child(specific_button)
                .on_click(move |selected, window, cx| {
                    let Some(index) = selected.first() else {
                        return;
                    };
                    if *index == 0 {
                        random_root.update(cx, |root, cx| match kind {
                            DeliveryFormKind::Send => {
                                root.set_send_broadcaster_choice(
                                    key,
                                    BroadcasterChoice::Random,
                                    cx,
                                );
                            }
                            DeliveryFormKind::Unshield => {
                                root.set_unshield_broadcaster_choice(
                                    key,
                                    BroadcasterChoice::Random,
                                    cx,
                                );
                            }
                        });
                    } else {
                        modal_root.update(cx, |root, cx| {
                            root.open_broadcaster_picker(kind, key, window, cx);
                        });
                    }
                }),
        )
        .when(
            matches!(kind, DeliveryFormKind::Send)
                && should_show_fee_mode_toggle(kind, action_token, selected_fee_token),
            |settings| {
                settings.child(render_fee_mode_toggle(
                    fee_mode_root,
                    key,
                    kind,
                    DeliveryMode::PublicBroadcaster,
                    fee_mode,
                    generating,
                ))
            },
        );

    if candidates.is_empty() {
        return settings.child(app_muted_text(
            "No eligible broadcaster currently advertises this token.",
        ));
    }
    settings
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
    sponsorship_enabled: bool,
    sponsorship_unavailable_reason: Option<&'static str>,
    generating: bool,
    submit_enabled: bool,
    submit: impl Fn(&mut Window, &mut App) + Clone + 'static,
) -> gpui::Div {
    let random_root = root.clone();
    let funding_root = root.clone();
    let incentive_root = root.clone();
    let gas_fee_root = root;
    let custom_incentive_input_for_select = custom_incentive_input.clone();
    let funding = if show_sponsored_funding_choice {
        funding
    } else {
        SelfBroadcastFundingMode::PublicBalance
    };
    let privacy_icon_placement = self_broadcast_privacy_icon_placement(sponsorship_enabled);
    let selected_uuid = selected_uuid.map(str::to_owned);
    let selected_account = selected_uuid.as_deref().and_then(|uuid| {
        accounts
            .iter()
            .find(|account| account.public_account_uuid == uuid)
    });
    let random_disabled = generating
        || !accounts.iter().any(|account| {
            if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                return Some(account.public_account_uuid.as_str()) != selected_uuid.as_deref();
            }
            self_broadcast_gas_payer_random_candidate(
                account,
                selected_uuid.as_deref(),
                key.chain_id,
                balance_snapshot,
            )
        });
    let missing_selection = !accounts.is_empty() && selected_account.is_none();
    let selected_zero_balance = funding == SelfBroadcastFundingMode::PublicBalance
        && selected_uuid.as_deref().is_some_and(|uuid| {
            self_broadcast_native_balance_state(balance_snapshot, key.chain_id, uuid)
                == SelfBroadcastNativeBalanceState::Zero
        });

    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .when(show_sponsored_funding_choice, |this| {
            this.child(app_inline_control_row(
                "Gas funding",
                ButtonGroup::new(delivery_element_id(key, kind, "funding-toggle"))
                    .outline()
                    .compact()
                    .disabled(generating)
                    // `child` overwrites member disabled state with the group flag.
                    .children(vec![
                        app_segment_button(
                            delivery_element_id(key, kind, "funding-public"),
                            "Public balance",
                            funding == SelfBroadcastFundingMode::PublicBalance,
                            generating,
                            (privacy_icon_placement
                                == SelfBroadcastPrivacyIconPlacement::PublicFunding)
                                .then(|| {
                                    render_self_broadcast_privacy_icon(delivery_element_id(
                                        key,
                                        kind,
                                        "funding-public-privacy-warning",
                                    ))
                                }),
                        ),
                        app_segment_button(
                            delivery_element_id(key, kind, "funding-private"),
                            BLOCK_BUILDER_SPONSORSHIP_LABEL,
                            funding == SelfBroadcastFundingMode::PrivateSponsorship,
                            generating || sponsorship_unavailable_reason.is_some(),
                            Some(
                                render_private_action_info_icon(
                                    delivery_element_id(key, kind, "funding-private-info"),
                                    BLOCK_BUILDER_SPONSORSHIP_LABEL,
                                    BLOCK_BUILDER_SPONSORSHIP_TOOLTIP,
                                )
                                .into_any_element(),
                            ),
                        ),
                    ])
                    .on_click(move |selected, _window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let funding = if *index == 0 {
                            SelfBroadcastFundingMode::PublicBalance
                        } else {
                            SelfBroadcastFundingMode::PrivateSponsorship
                        };
                        funding_root.update(cx, |root, cx| {
                            root.set_self_broadcast_funding_mode(kind, key, funding, cx);
                        });
                    }),
            ))
        })
        .when_some(sponsorship_unavailable_reason, |this, reason| {
            this.child(app_muted_text(reason))
        })
        .when(
            funding == SelfBroadcastFundingMode::PrivateSponsorship,
            |this| {
                this.child(app_inline_control_row(
                    "Builder incentive",
                    ButtonGroup::new(delivery_element_id(key, kind, "incentive-toggle"))
                        .outline()
                        .compact()
                        .disabled(generating)
                        .children(vec![
                            app_segment_button(
                                delivery_element_id(key, kind, "incentive-economy"),
                                "Economy 1%",
                                incentive == SponsoredIncentive::Economy,
                                generating,
                                None,
                            ),
                            app_segment_button(
                                delivery_element_id(key, kind, "incentive-standard"),
                                "Standard 5%",
                                incentive == SponsoredIncentive::Standard,
                                generating,
                                None,
                            ),
                            app_segment_button(
                                delivery_element_id(key, kind, "incentive-priority"),
                                "Priority 15%",
                                incentive == SponsoredIncentive::Priority,
                                generating,
                                None,
                            ),
                            app_segment_button(
                                delivery_element_id(key, kind, "incentive-custom"),
                                "Custom",
                                matches!(incentive, SponsoredIncentive::Custom(_)),
                                generating,
                                None,
                            ),
                        ])
                        .on_click(move |selected, _window, cx| {
                            let Some(index) = selected.first() else {
                                return;
                            };
                            let incentive = match *index {
                                0 => SponsoredIncentive::Economy,
                                1 => SponsoredIncentive::Standard,
                                2 => SponsoredIncentive::Priority,
                                3 => {
                                    let Ok(percent) = custom_incentive_input_for_select
                                        .read(cx)
                                        .value()
                                        .trim()
                                        .parse::<u8>()
                                    else {
                                        return;
                                    };
                                    SponsoredIncentive::Custom(percent)
                                }
                                _ => return,
                            };
                            incentive_root.update(cx, |root, cx| {
                                root.set_sponsored_incentive(kind, key, incentive, cx);
                            });
                        }),
                ))
                .when(
                    matches!(incentive, SponsoredIncentive::Custom(_)),
                    |this| {
                        this.child(app_inline_control_row(
                            "Custom incentive (1-100%)",
                            crate::root::ui_helpers::input_enter_scope(
                                submit_enabled,
                                submit.clone(),
                            )
                            .child(
                                private_action_input(custom_incentive_input)
                                    .disabled(generating)
                                    .w(px(180.0)),
                            ),
                        ))
                    },
                )
            },
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div().min_w(px(0.0)).flex().flex_col().gap_1().child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(app_muted_text(
                                if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                                    "Transaction signer"
                                } else {
                                    "Gas payer"
                                },
                            ))
                            .when(selected_zero_balance, |this| {
                                this.child(render_self_broadcast_gas_payer_warning_icon(
                                    delivery_element_id(key, kind, "zero-gas-payer-warning"),
                                ))
                            }),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_button_base(delivery_element_id(key, kind, "random-gas-payer"))
                                .accessibility_label(
                                    if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                                        "Choose random transaction signer"
                                    } else {
                                        "Choose random gas payer"
                                    },
                                )
                                .icon(Icon::new(RailgunActionIcon::Dices))
                                .ghost()
                                .small()
                                .compact()
                                .tooltip(
                                    if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                                        "Choose random transaction signer"
                                    } else {
                                        "Choose random gas payer"
                                    },
                                )
                                .disabled(random_disabled)
                                .on_click(move |_event, window, cx| {
                                    random_root.update(cx, |root, cx| {
                                        root.choose_random_self_broadcast_gas_payer(
                                            kind, key, window, cx,
                                        );
                                    });
                                }),
                        )
                        .child(
                            div().w(px(320.0)).h(px(32.0)).child(
                                Select::new(gas_payer_select)
                                    .small()
                                    .w_full()
                                    .h(px(32.0))
                                    .placeholder(if missing_selection {
                                        if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                                            "Transaction signer required"
                                        } else {
                                            "Gas payer required"
                                        }
                                    } else {
                                        "Please select"
                                    })
                                    .menu_width(px(380.0))
                                    .when(missing_selection || selected_zero_balance, |this| {
                                        this.border_color(rgb(theme::DANGER))
                                    })
                                    .disabled(generating || accounts.is_empty()),
                            ),
                        ),
                ),
        )
        .when(accounts.is_empty(), |this| {
            this.child(app_muted_text(
                if funding == SelfBroadcastFundingMode::PrivateSponsorship {
                    "No active Public accounts are available as transaction signers."
                } else {
                    "No active Public accounts are available for self-broadcast gas payment."
                },
            ))
        })
        .child(
            crate::root::ui_helpers::input_enter_scope(submit_enabled, submit).child(
                render_eip1559_gas_fee_editor(
                    gas_fee_root,
                    &Eip1559GasFeeTarget::Private { key, kind },
                    gas_fee,
                    generating,
                ),
            ),
        )
}

pub(in crate::root) fn render_sponsored_funding_estimate(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    display: &SponsoredFundingEstimateDisplay,
    breakdown_open: bool,
) -> gpui::Div {
    let estimate = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Estimated fees"));
    match display {
        SponsoredFundingEstimateDisplay::PublicBalance(display) => {
            render_public_action_fee_estimate(display, false)
        }
        SponsoredFundingEstimateDisplay::PublicBalanceError => estimate.child(
            Alert::error(
                delivery_element_id(key, kind, "public-funding-estimate-error"),
                "Fee estimate is unavailable for the current inputs.",
            )
            .small(),
        ),
        SponsoredFundingEstimateDisplay::Ready {
            expected_sponsorship_cost,
            gas_cost,
            builder_premium,
            primary_unshield_protocol_fee,
            expected_excess_deposit,
            maximum_spend,
            show_excess_deposit_breakdown,
        } => estimate
            .child(render_sponsored_funding_breakdown(
                root,
                key,
                kind,
                expected_sponsorship_cost.clone(),
                gas_cost.clone(),
                builder_premium.clone(),
                expected_excess_deposit,
                maximum_spend.clone(),
                *show_excess_deposit_breakdown,
                breakdown_open,
            ))
            .when_some(
                primary_unshield_protocol_fee.clone(),
                |this, protocol_fee| {
                    this.child(sponsored_funding_estimate_row(
                        public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS),
                        protocol_fee,
                    ))
                },
            ),
        SponsoredFundingEstimateDisplay::Error(error) => estimate.child(
            Alert::error(
                delivery_element_id(key, kind, "sponsored-estimate-error"),
                error.clone(),
            )
            .small(),
        ),
    }
}

fn render_sponsored_funding_breakdown(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    expected_sponsorship_cost: String,
    gas_cost: String,
    builder_premium: String,
    expected_excess_deposit: &str,
    maximum_spend: String,
    show_excess_deposit_breakdown: bool,
    open: bool,
) -> Collapsible {
    Collapsible::new()
        .open(open)
        .w_full()
        .rounded_md()
        .overflow_hidden()
        .child(
            div()
                .id(delivery_element_id(
                    key,
                    kind,
                    "sponsored-funding-breakdown",
                ))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .py(px(5.0))
                .cursor_pointer()
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    root.update(cx, |root, cx| {
                        root.set_sponsored_funding_breakdown_open(kind, key, !open, cx);
                    });
                })
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::TEXT))
                        .child("Expected transaction cost"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .child(
                            app_strong_text(expected_sponsorship_cost)
                                .min_w(px(0.0))
                                .text_size(px(13.0))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .whitespace_normal(),
                        )
                        .child(
                            Icon::new(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .text_color(rgb(theme::TEXT_MUTED)),
                        ),
                ),
        )
        .content(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px(px(10.0))
                .py(px(8.0))
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .child(sponsored_funding_detail_row(
                    "Expected network gas",
                    gas_cost,
                ))
                .child(sponsored_funding_detail_row(
                    "Builder premium",
                    builder_premium,
                ))
                .when(show_excess_deposit_breakdown, |this| {
                    this.child(sponsored_funding_estimate_divider())
                        .child(sponsored_funding_detail_row(
                            "Unshielded up front",
                            maximum_spend,
                        ))
                        .child(sponsored_funding_detail_row(
                            "Excess deposited to signer",
                            expected_excess_deposit.to_string(),
                        ))
                        .child(
                            app_muted_text(
                                "Unused builder funding remains on the signer as public ETH.",
                            )
                            .text_size(px(12.0))
                            .line_height(px(15.0)),
                        )
                }),
        )
}

fn sponsored_funding_estimate_divider() -> gpui::Div {
    div()
        .my(px(3.0))
        .border_t_1()
        .border_color(rgb(theme::BORDER))
}

fn sponsored_funding_detail_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .text_color(rgb(theme::TEXT))
        .text_size(px(12.0))
        .line_height(px(15.0))
        .child(label)
        .child(
            div()
                .min_w(px(0.0))
                .font_family(APP_MONO_FONT_FAMILY)
                .whitespace_normal()
                .child(value),
        )
}

fn sponsored_funding_estimate_row(label: impl Into<SharedString>, value: String) -> gpui::Div {
    let label: SharedString = label.into();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(div().flex_none().text_color(rgb(theme::TEXT)).child(label))
        .child(
            app_strong_text(value)
                .min_w(px(0.0))
                .text_size(px(13.0))
                .font_family(APP_MONO_FONT_FAMILY)
                .whitespace_normal(),
        )
}

pub(in crate::root) fn self_broadcast_gas_payer_select_trigger_row(
    label: &str,
    address: &Address,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(SharedString::from(label.to_string()))
        .child(
            app_muted_text(short_address(address))
                .font_family(APP_FONT_FAMILY)
                .text_size(px(12.0)),
        )
}

pub(in crate::root) fn self_broadcast_gas_payer_select_menu_row(
    label: &str,
    address: &Address,
    chain_id: u64,
    balance: &str,
) -> gpui::Div {
    div()
        .w_full()
        .py_1()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(label.to_string()))
                .child(
                    app_muted_text(short_address(address))
                        .font_family(APP_FONT_FAMILY)
                        .text_color(rgb(theme::TEXT_MUTED)),
                ),
        )
        .child(
            app_muted_text(format!(
                "{balance} {}",
                native_token_display_label(chain_id)
            ))
            .debug_selector(|| format!("gas-payer-balance-{label}"))
            .text_color(rgb(theme::TEXT_MUTED))
            .flex_none()
            .text_right(),
        )
}

pub(in crate::root) fn render_danger_switch(
    id: SharedString,
    checked: bool,
    disabled: bool,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    render_switch(id, checked, disabled, theme::DANGER, on_toggle)
}

pub(in crate::root) fn render_switch(
    id: SharedString,
    checked: bool,
    disabled: bool,
    checked_color: u32,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let track_width = px(36.0);
    let track_height = px(20.0);
    let thumb_size = px(16.0);
    let inset = px(2.0);
    let max_x = track_width - thumb_size - inset * 2.0;
    let thumb_x = if checked { max_x } else { px(0.0) };
    let track_color = if checked {
        checked_color
    } else {
        theme::SURFACE_HOVER
    };
    let thumb_color = if checked {
        theme::SURFACE
    } else {
        theme::TEXT_MUTED
    };

    div()
        .id(id)
        .w(track_width)
        .h(track_height)
        .flex()
        .items_center()
        .p(inset)
        .rounded_full()
        .bg(rgb(track_color))
        .opacity(if disabled { 0.5 } else { 1.0 })
        .when(!disabled, |this| {
            this.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                on_toggle(!checked, window, cx);
            })
        })
        .child(
            div()
                .size(thumb_size)
                .rounded_full()
                .bg(rgb(thumb_color))
                .left(thumb_x)
                .with_animation(
                    ElementId::NamedInteger("danger-switch-thumb".into(), u64::from(checked)),
                    Animation::new(Duration::from_secs_f64(0.15)),
                    move |this, delta| {
                        let x = if checked {
                            max_x * delta
                        } else {
                            max_x - max_x * delta
                        };
                        this.left(x)
                    },
                ),
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
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Output"))
        .child(
            ButtonGroup::new(unshield_element_id(key, "output-toggle"))
                .outline()
                .disabled(generating)
                .child(
                    app_button(unshield_element_id(key, "output-native"), native_label)
                        .selected(unwrap)
                        .disabled(generating),
                )
                .child(
                    app_button(unshield_element_id(key, "output-wrapped"), wrapped_label)
                        .selected(!unwrap)
                        .disabled(generating),
                )
                .on_click(move |selected, _window, cx| {
                    let Some(index) = selected.first() else {
                        return;
                    };
                    let unwrap = *index == 0;
                    root.update(cx, |root, cx| {
                        root.set_unshield_unwrap(key, unwrap, cx);
                    });
                }),
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
