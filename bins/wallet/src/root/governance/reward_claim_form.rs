use super::*;

/// The claim selection form: the account's positive rewards with their fee and net value, the
/// totals for the exact selected call, and Review into the existing preflight.
pub(super) fn render_reward_claim_selection_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    selection: &StakingActionSelection,
    tokens: &[Address],
    content_width: gpui::Pixels,
    cx: &App,
) -> gpui::Div {
    let chain_id = wallet.last_evm_chain;
    let uuid = selection.actor_uuid.as_str();
    let state = &wallet.governance.staking;
    let anchor_cache = &wallet.public_broadcaster_anchor_cache;
    let action_flow = &wallet.governance.action_flow;
    let registry = &wallet.effective_token_registry;
    let gas_fee = &wallet.governance.reward_gas_fee;
    let gas_selection = gas_fee.selection(cx);
    let estimate = action_flow.reward_selection_estimate.as_ref();
    let estimated = match estimate {
        Some(RewardSelectionEstimateState::Estimated(estimate)) => Some(estimate),
        _ => None,
    };
    let selected_fee_per_gas =
        estimated
            .zip(gas_selection.as_ref().ok())
            .map(|(estimate, selection)| {
                reward_claim_fee_per_gas(*selection, gas_fee.quote, estimate.expected_fee_per_gas)
            });
    let rows = governance_contracts(chain_id)
        .map_or(&[][..], |contracts| contracts.reward_tokens)
        .iter()
        .filter_map(|entry| {
            let token = entry.token;
            let resolved = state.resolved_reward(uuid, token);
            let Some(RewardView::Positive { amount, .. }) = resolved else {
                return None;
            };
            let metadata = token_display_metadata(Some(registry), chain_id, &token);
            let usd = reward_usd_state(chain_id, token, resolved, anchor_cache);
            let fee = match (estimated, selected_fee_per_gas) {
                (Some(estimate), Some(price)) => reward_added_fee_presentation(
                    chain_id,
                    estimate,
                    token,
                    price,
                    usd,
                    anchor_cache,
                ),
                _ if gas_selection.is_ok()
                    && matches!(estimate, Some(RewardSelectionEstimateState::Pending)) =>
                {
                    RewardFeePresentation::Pending
                }
                _ => RewardFeePresentation::Unavailable,
            };
            Some(RewardSelectionRow {
                token,
                symbol: metadata
                    .as_ref()
                    .map_or_else(|| entry.symbol.to_owned(), |info| info.symbol.clone()),
                icon_path: metadata.and_then(|info| info.icon_path),
                amount: format_reward_amount(chain_id, token, *amount, registry),
                usd,
                fee,
                gas: estimated
                    .and_then(|estimate| estimate.added_gas.get(&token).copied().flatten()),
                selected: tokens.contains(&token),
            })
        })
        .collect::<Vec<_>>();
    let selected_count = rows.iter().filter(|row| row.selected).count();
    let all_selected = !rows.is_empty() && selected_count == rows.len();
    let selected_usd = rows
        .iter()
        .filter(|row| row.selected)
        .try_fold(U256::ZERO, |total, row| match row.usd {
            RewardUsdState::Value(value) => total.checked_add(value),
            RewardUsdState::Loading | RewardUsdState::Unavailable => None,
        });
    let selected_value = format!(
        "{selected_count} of {} · {}",
        rows.len(),
        if selected_count == 0 {
            "—".to_owned()
        } else {
            selected_usd.map_or_else(|| "USD unavailable".to_owned(), format_usd_micro_value)
        }
    );
    let selected_cost = estimated
        .zip(selected_fee_per_gas)
        .and_then(|(estimate, price)| reward_selection_cost(&estimate.steps, price))
        .filter(|_| !tokens.is_empty());
    let fee_usd = selected_cost
        .and_then(|cost| anchor_cache.cached_native_usd_micro_value(chain_id, cost))
        .filter(|_| selected_count > 0);
    let header_root = root.clone();
    let header_tokens = rows.iter().map(|row| row.token).collect::<Vec<_>>();
    let mut list = div()
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(rgb(theme::SURFACE_ELEVATED))
                .child(
                    div().w(REWARD_SELECTION_CHECKBOX_WIDTH).flex_none().child(
                        Checkbox::new("governance-reward-select-all")
                            .checked(all_selected)
                            .on_click(move |checked: &bool, _window, cx| {
                                let tokens = if *checked {
                                    header_tokens.clone()
                                } else {
                                    Vec::new()
                                };
                                header_root.update(cx, |root, cx| {
                                    root.set_reward_claim_selection_all(tokens, cx);
                                });
                            }),
                    ),
                )
                .child(
                    app_muted_text("Asset")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_ASSET_WIDTH)
                        .flex_none(),
                )
                .child(
                    app_muted_text("Unclaimed")
                        .text_size(px(11.0))
                        .flex_1()
                        .min_w(px(0.0)),
                )
                .child(
                    app_muted_text("Added fee")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_FEE_WIDTH)
                        .flex_none()
                        .text_right(),
                )
                .child(
                    app_muted_text("Net")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_NET_WIDTH)
                        .flex_none()
                        .text_right(),
                ),
        );
    for row in rows {
        let toggle_root = root.clone();
        let token = row.token;
        let selected = row.selected;
        let (row_fee_usd, below_fee) = match &row.fee {
            RewardFeePresentation::Estimated(fee) => (fee.usd, fee.below_fee),
            RewardFeePresentation::Pending | RewardFeePresentation::Unavailable => (None, false),
        };
        let dimmed = below_fee && !selected;
        let fee_cell = match row.fee {
            RewardFeePresentation::Pending => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text("estimating…")
                        .text_size(px(11.0))
                        .whitespace_nowrap(),
                ),
            RewardFeePresentation::Estimated(fee) => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .child(app_text(format!("≈ {}", fee.label)).whitespace_nowrap())
                .children(row.gas.map(|gas| {
                    app_muted_text(format!("{} gas", compact_gas(gas)))
                        .text_size(px(11.0))
                        .whitespace_nowrap()
                })),
            RewardFeePresentation::Unavailable => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .child(
                    app_muted_text("unavailable")
                        .text_size(px(11.0))
                        .whitespace_nowrap(),
                ),
        };
        let net_cell = match (row.usd, row_fee_usd) {
            (RewardUsdState::Value(reward_usd), Some(row_fee_usd)) => {
                let (label, negative) = reward_net_label(reward_usd, row_fee_usd);
                app_text(label)
                    .whitespace_nowrap()
                    .when(negative, |value| value.text_color(rgb(theme::DANGER)))
            }
            _ => app_muted_text("—"),
        };
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .border_t_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .when(dimmed, |row| row.opacity(0.6))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(REWARD_SELECTION_CHECKBOX_WIDTH)
                                .flex_none()
                                .child(
                                    Checkbox::new(staking_control_id(
                                        uuid,
                                        "reward-select",
                                        token,
                                    ))
                                    .checked(selected)
                                    .on_click(move |checked: &bool, _window, cx| {
                                        let checked = *checked;
                                        toggle_root.update(cx, |root, cx| {
                                            root.set_reward_claim_selection_token(
                                                token, checked, cx,
                                            );
                                        });
                                    }),
                                ),
                        )
                        .child(
                            token_label_row(
                                SharedString::from(row.symbol),
                                row.icon_path,
                                px(16.0),
                            )
                            .w(REWARD_SELECTION_ASSET_WIDTH)
                            .flex_none()
                            .truncate(),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .child(app_strong_text(row.amount).whitespace_nowrap())
                                .child(
                                    app_muted_text(match row.usd {
                                        RewardUsdState::Value(value) => {
                                            format_usd_micro_value(value)
                                        }
                                        RewardUsdState::Loading | RewardUsdState::Unavailable => {
                                            "USD unavailable".to_owned()
                                        }
                                    })
                                    .text_size(px(11.0))
                                    .whitespace_nowrap(),
                                ),
                        )
                        .child(fee_cell)
                        .child(
                            div()
                                .w(REWARD_SELECTION_NET_WIDTH)
                                .flex_none()
                                .flex()
                                .justify_end()
                                .child(net_cell),
                        ),
                )
                .when(below_fee, |row| {
                    row.child(
                        app_muted_text(
                            "Added fee exceeds the reward. Rewards keep accruing and can be claimed later.",
                        )
                        .text_size(px(11.0))
                        .whitespace_normal(),
                    )
                }),
        );
    }
    let split_notice = estimated.filter(|estimate| estimate.steps.len() > 1).map(|estimate| {
        let intervals = estimate
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.ending_interval.checked_sub(evidence.starting_interval))
            .and_then(|span| span.checked_add(U256::ONE));
        Alert::warning(
            "governance-reward-selection-split",
            intervals.map_or_else(
                || {
                    format!(
                        "This claim does not fit one block. It will be sent as {} transactions, each needing its own signature.",
                        estimate.steps.len()
                    )
                },
                |intervals| {
                    format!(
                        "This claim covers {intervals} intervals and does not fit one block. It will be sent as {} transactions, each needing its own signature.",
                        estimate.steps.len()
                    )
                },
            ),
        )
        .small()
    });
    let fee_value = match (estimate, selected_fee_per_gas, selected_cost) {
        _ if tokens.is_empty() => app_muted_text("—").into_any_element(),
        (None, _, _) => app_muted_text("—").into_any_element(),
        (Some(RewardSelectionEstimateState::Pending), _, _) => div()
            .flex()
            .items_center()
            .gap_1()
            .child(Spinner::new().small())
            .child(app_muted_shimmer_text("estimating…"))
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Unavailable(reason)), _, _) => div()
            .flex()
            .flex_col()
            .child(app_text("unavailable"))
            .child(
                app_muted_text(reason.to_string())
                    .text_size(px(11.0))
                    .whitespace_normal(),
            )
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Estimated(estimate)), Some(price), Some(cost)) => div()
            .flex()
            .items_baseline()
            .justify_end()
            .gap_1p5()
            .child(
                app_muted_text(reward_selection_gas_summary(&estimate.steps, price))
                    .text_size(px(11.0))
                    .whitespace_nowrap(),
            )
            .child(
                app_text(format!(
                    "≈ {}",
                    fee_usd.map_or_else(
                        || crate::root::tokens::format_native_token_amount_for_display(
                            chain_id, cost
                        ),
                        format_usd_micro_value
                    )
                ))
                .whitespace_nowrap(),
            )
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Estimated(_)), _, _) => {
            app_muted_text("unavailable").into_any_element()
        }
    };
    let net_value = match (selected_usd, fee_usd) {
        (Some(reward_usd), Some(fee_usd)) => {
            let (label, negative) = reward_net_label(reward_usd, fee_usd);
            app_strong_text(format!("≈ {label}"))
                .when(negative, |value| value.text_color(rgb(theme::DANGER)))
                .into_any_element()
        }
        _ => app_muted_text("—").into_any_element(),
    };
    let shared_fee = estimated
        .zip(selected_fee_per_gas)
        .and_then(|(estimate, price)| {
            estimate.shared_gas.map(|gas| {
                let fee = reward_cost_presentation(
                    chain_id,
                    U256::from(gas) * U256::from(price),
                    RewardUsdState::Unavailable,
                    anchor_cache,
                );
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(app_muted_text(format!("{} gas", compact_gas(gas))).text_size(px(11.0)))
                    .child(app_text(format!("≈ {}", fee.label)))
                    .into_any_element()
            })
        })
        .unwrap_or_else(|| {
            app_muted_text(
                if matches!(estimate, Some(RewardSelectionEstimateState::Pending)) {
                    "Estimating…"
                } else {
                    "Unavailable"
                },
            )
            .into_any_element()
        });
    // Totals read as label left, value right, like the authorization summary rows.
    let totals_value = |value: AnyElement| {
        div()
            .w_full()
            .flex()
            .justify_end()
            .text_right()
            .child(value)
            .into_any_element()
    };
    let review_root = root.clone();
    let cancel_root = root.clone();
    let pending = action_flow.pending;
    let review_ready = !tokens.is_empty() && selected_cost.is_some() && !pending;
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(list)
        .child(render_eip1559_gas_fee_editor(
            root.clone(),
            &Eip1559GasFeeTarget::Governance,
            gas_fee,
            pending,
        ))
        .children(split_notice)
        .child(
            DescriptionList::horizontal()
                .bordered(false)
                .columns(1)
                .label_width(px(120.0))
                .child(
                    DescriptionItem::new(app_muted_text("Selected").into_any_element())
                        .value(totals_value(app_text(selected_value).into_any_element())),
                )
                .child(
                    DescriptionItem::new(app_muted_text("Shared fee").into_any_element())
                        .value(totals_value(shared_fee)),
                )
                .child(
                    DescriptionItem::new(app_muted_text("Estimated fee").into_any_element())
                        .value(totals_value(fee_value)),
                )
                .separator()
                .child(
                    DescriptionItem::new(app_strong_text("Net").into_any_element())
                        .value(totals_value(net_value)),
                ),
        )
        .children(action_flow.error.as_ref().map(|error| {
            Alert::error("governance-staking-action-error", error.to_string()).small()
        }))
        .child(if pending {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(8.0))
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text("Refreshing rewards and estimating the exact call…")
                        .text_size(px(12.0)),
                )
        } else {
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    app_button_base("governance-staking-action-cancel")
                        .ghost()
                        .small()
                        .child("Cancel")
                        .on_click(move |_event, window, cx| {
                            cancel_root.update(cx, WalletRoot::close_staking_action);
                            window.close_dialog(cx);
                        }),
                )
                .child(
                    app_button_base("governance-staking-action-review")
                        .primary()
                        .small()
                        .disabled(!review_ready)
                        .child("Review")
                        .on_click(move |_event, window, cx| {
                            review_root.update(cx, |root, cx| {
                                root.review_staking_action(window, cx);
                            });
                        }),
                )
        })
}
