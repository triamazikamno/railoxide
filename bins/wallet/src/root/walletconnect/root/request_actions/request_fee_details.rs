use super::*;

pub(super) fn render_walletconnect_request_disclosure(
    root: &Entity<WalletRoot>,
    request_key: &str,
    disclosure: WalletConnectRequestDisclosure,
    label: &'static str,
    open: bool,
    content: gpui::Div,
) -> Collapsible {
    let toggle_root = root.clone();
    let toggle_key = request_key.to_owned();
    let id = format!("walletconnect-request-{request_key}-{label}-disclosure");
    let toggle = app_button_base(SharedString::from(id))
        .text()
        .small()
        .compact()
        .icon(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .text_color(rgb(theme::TEXT_MUTED))
        .child(app_button_label(label))
        .on_click(move |_event, _window, cx| {
            cx.stop_propagation();
            toggle_root.update(cx, |root, cx| {
                root.walletconnect
                    .toggle_request_disclosure(&toggle_key, disclosure);
                cx.notify();
            });
        });
    Collapsible::new()
        .open(open)
        .w_full()
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(toggle),
        )
        .content(content)
}

pub(super) fn render_walletconnect_transaction_details(
    details: &crate::root::walletconnect::intent::WalletConnectTransactionDetails<'_>,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let transaction = details.transaction;
    let mut content = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    if let Some(target) = transaction.to {
        content = content.child(render_walletconnect_address_row(
            "Target",
            None,
            target,
            Some("Contract target"),
            request_key,
            content_width,
        ));
    } else {
        content = content.child(walletconnect_kv_row(
            "Target",
            "Contract creation".to_owned(),
        ));
    }
    content = content
        .child(walletconnect_kv_row(
            "Chain ID",
            details.chain_id.to_string(),
        ))
        .child(walletconnect_kv_row(
            "Native value",
            transaction
                .value
                .map_or_else(|| "0".to_owned(), |value| value.to_string()),
        ))
        .child(walletconnect_kv_row(
            "Calldata selector",
            walletconnect_transaction_selector(transaction).map_or_else(
                || "None".to_owned(),
                |selector| format!("0x{}", alloy::hex::encode(selector)),
            ),
        ));
    content = content.child(walletconnect_kv_row(
        "Calldata",
        walletconnect_calldata_context(transaction),
    ));
    if let Some(value) = transaction.gas {
        content = content.child(walletconnect_kv_row(
            "Dapp gas (not used)",
            value.to_string(),
        ));
    }
    if let Some(value) = transaction.gas_price {
        content = content.child(walletconnect_kv_row(
            "Dapp gas price (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.max_fee_per_gas {
        content = content.child(walletconnect_kv_row(
            "Dapp max fee (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.max_priority_fee_per_gas {
        content = content.child(walletconnect_kv_row(
            "Dapp max tip (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.nonce {
        content = content.child(walletconnect_kv_row(
            "Dapp nonce (not used)",
            value.to_string(),
        ));
    }
    if let Some(value) = transaction.transaction_type {
        content = content.child(walletconnect_kv_row(
            "Dapp transaction type (not used)",
            value.to_string(),
        ));
    }
    walletconnect_disclosure_content(content)
}

pub(super) fn render_walletconnect_network_fee(
    native_currency: &railgun_ui::NativeCurrency,
    root: &Entity<WalletRoot>,
    request: &WalletConnectRequestUi,
    fee_state: Option<&crate::root::walletconnect::fee::WalletConnectFeeState>,
    refreshing: bool,
    open: bool,
    fee_editor: &Eip1559GasFeeEditorState,
    significance: Option<crate::root::walletconnect::intent::WalletConnectFeeSignificance>,
) -> gpui::Div {
    let request_key = request.key.clone();
    let status = fee_state.map(|state| state.status);
    let error = fee_state
        .and_then(|state| state.error.as_ref())
        .map(ToString::to_string);
    let mut row = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    let mut value = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .text_align(gpui::TextAlign::Right)
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(1.0));
    let simulation_disclosure = status == Some(WalletConnectFeeStatus::AwaitingSimulation);
    let simulation_in_flight = fee_state.is_some_and(|state| state.simulation_requested);
    if let Some(projection) =
        fee_state.and_then(|state| walletconnect_fee_state_projection(state, refreshing))
    {
        value = value.child(walletconnect_fee_cost_value(
            native_currency,
            projection.expected_gas_cost,
            projection.expected_native_usd_micro_value,
            significance,
            false,
        ));
    } else {
        match status {
            Some(WalletConnectFeeStatus::WouldRevert) => {
                value = value.child(
                    div()
                        .text_size(APP_TEXT_SIZE)
                        .line_height(relative(1.0))
                        .text_color(rgb(theme::DANGER))
                        .child("Would revert"),
                );
            }
            Some(WalletConnectFeeStatus::AwaitingSimulation) => {}
            Some(WalletConnectFeeStatus::UnavailableFailed) => {
                value = value.child(app_muted_text("Unavailable"));
            }
            Some(WalletConnectFeeStatus::Fetching) | None => {
                value = value.child(app_muted_shimmer_text("Fetching fee data..."));
            }
            Some(
                WalletConnectFeeStatus::EstimatedFromOperation(_)
                | WalletConnectFeeStatus::Simulated(_),
            ) => unreachable!("typed fee projections are handled above"),
        }
    }
    let can_simulate = walletconnect_request_can_simulate(request)
        && !matches!(
            status,
            Some(WalletConnectFeeStatus::Simulated(_) | WalletConnectFeeStatus::WouldRevert)
        );
    let toggle_root = root.clone();
    let toggle_key = request_key.clone();
    let toggle = app_button_base(SharedString::from(format!(
        "walletconnect-network-fee-toggle-{request_key}"
    )))
    .text()
    .small()
    .compact()
    .icon(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .text_color(rgb(theme::TEXT_MUTED))
    .child(app_button_label(WALLETCONNECT_NETWORK_FEE_LABEL))
    .on_click(move |_event, _window, cx| {
        cx.stop_propagation();
        toggle_root.update(cx, |root, cx| {
            root.walletconnect
                .toggle_request_disclosure(&toggle_key, WalletConnectRequestDisclosure::NetworkFee);
            cx.notify();
        });
    });
    let mut right_group = div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .child(value);
    let simulation_action_visible = can_simulate
        && (simulation_disclosure || fee_state.is_none_or(|state| !state.simulation_retryable));
    if simulation_action_visible {
        let simulate_root = root.clone();
        let key = request_key.clone();
        right_group = right_group.child(
            app_button(
                SharedString::from(format!("walletconnect-simulate-{request_key}")),
                "Simulate on network",
            )
            .outline()
            .small()
            .loading(simulation_in_flight)
            .child(gpui_component::Icon::new(IconName::Info).with_size(px(14.0)))
            .tooltip("Simulation sends transaction details to the configured RPC.")
            .disabled(simulation_in_flight)
            .on_click(move |_event, _window, cx| {
                cx.stop_propagation();
                simulate_root.update(cx, |root, cx| {
                    root.simulate_current_walletconnect_transaction(&key, cx);
                });
            }),
        );
    }
    if matches!(status, Some(WalletConnectFeeStatus::UnavailableFailed)) {
        let retry_root = root.clone();
        let key = Arc::<str>::from(request_key.as_str());
        let retrying = fee_state.is_some_and(|state| {
            walletconnect_fee_retrying(state.retry_attempt, refreshing, state.simulation_requested)
        });
        let retry_blocked = !fee_state.is_some_and(|state| {
            walletconnect_fee_retry_action_enabled(
                state.status,
                refreshing,
                state.simulation_requested,
            )
        });
        right_group = right_group.child(
            app_button(
                SharedString::from(format!("walletconnect-retry-fee-{request_key}")),
                "Retry",
            )
            .outline()
            .small()
            .loading(retrying)
            .disabled(retry_blocked)
            .on_click(move |_event, _window, cx| {
                cx.stop_propagation();
                let key = Arc::clone(&key);
                retry_root.update(cx, |root, cx| {
                    root.retry_walletconnect_fee(key.as_ref(), cx);
                });
            }),
        );
    }
    let simulation_failed = fee_state.is_some_and(|state| {
        matches!(state.status, WalletConnectFeeStatus::UnavailableFailed)
            && state.simulation_retryable
    });
    row = row.child(
        Collapsible::new()
            .open(open)
            .w_full()
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(toggle)
                    .child(right_group),
            )
            .content(render_walletconnect_fee_details(
                native_currency,
                root.clone(),
                request,
                fee_state,
                refreshing,
                fee_editor,
                significance,
            )),
    );
    if simulation_failed && let Some(error) = error.filter(|error| !error.is_empty()) {
        row = row.child(
            Alert::error(
                SharedString::from(format!("walletconnect-simulation-error-{request_key}")),
                error,
            )
            .small(),
        );
    }
    if let Some(message) = significance
        .and_then(crate::root::walletconnect::intent::WalletConnectFeeSignificance::warning_message)
    {
        row = row.child(
            Alert::warning(
                SharedString::from(format!("walletconnect-fee-significance-{request_key}")),
                message,
            )
            .small(),
        );
    }
    row
}

fn render_walletconnect_fee_details(
    native_currency: &railgun_ui::NativeCurrency,
    root: Entity<WalletRoot>,
    request: &WalletConnectRequestUi,
    fee_state: Option<&crate::root::walletconnect::fee::WalletConnectFeeState>,
    refreshing: bool,
    fee_editor: &Eip1559GasFeeEditorState,
    significance: Option<crate::root::walletconnect::intent::WalletConnectFeeSignificance>,
) -> gpui::Div {
    let raw_gas_limit = walletconnect_request_raw_gas(request);
    let mut details = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    if let Some(projection) =
        fee_state.and_then(|state| walletconnect_fee_state_projection(state, refreshing))
    {
        details = details
            .child(walletconnect_kv_element_row(
                "Gas usage",
                app_muted_text(format_gas_limit(projection.raw_gas_limit)),
            ))
            .child(walletconnect_kv_element_row(
                "Expected cost",
                walletconnect_fee_cost_value(
                    native_currency,
                    projection.expected_gas_cost,
                    projection.expected_native_usd_micro_value,
                    significance,
                    false,
                ),
            ))
            .when(
                wallet_ops::public_action_maximum_gas_cost_is_significant(
                    projection.expected_gas_cost,
                    projection.maximum_gas_cost,
                ),
                |this| {
                    this.child(walletconnect_kv_element_row(
                        "Maximum cost",
                        walletconnect_fee_cost_value(
                            native_currency,
                            projection.maximum_gas_cost,
                            projection.maximum_native_usd_micro_value,
                            None,
                            true,
                        ),
                    ))
                },
            );
    } else {
        details = details
            .child(walletconnect_kv_element_row(
                "Gas usage",
                app_muted_text(
                    raw_gas_limit
                        .map_or_else(|| "Requires simulation".to_owned(), format_gas_limit),
                ),
            ))
            .child(walletconnect_kv_element_row(
                "Expected cost",
                app_muted_text("Needs gas usage"),
            ));
    }
    details = details.child(div().pt(px(6.0)).child(render_eip1559_gas_fee_editor(
        root,
        &Eip1559GasFeeTarget::WalletConnect {
            request_key: Arc::from(request.key.as_str()),
        },
        fee_editor,
        false,
    )));
    walletconnect_disclosure_content(details)
}

pub(super) fn walletconnect_dapp_gas_price(value: U256) -> String {
    if value <= U256::from(u128::MAX) {
        format!("{} gwei (not used)", format_gwei(value.to::<u128>()))
    } else {
        format!("{value} wei (not used)")
    }
}

fn walletconnect_fee_cost_value(
    native_currency: &railgun_ui::NativeCurrency,
    cost: alloy::primitives::U256,
    usd: Option<alloy::primitives::U256>,
    significance: Option<crate::root::walletconnect::intent::WalletConnectFeeSignificance>,
    muted_all: bool,
) -> gpui::Div {
    let native = native_currency.format_amount(cost);
    let warning = significance.is_some_and(|significance| {
        matches!(
            significance,
            crate::root::walletconnect::intent::WalletConnectFeeSignificance::High
                | crate::root::walletconnect::intent::WalletConnectFeeSignificance::ExceedsAmount
        )
    });
    let usd_color = if muted_all {
        theme::TEXT_MUTED
    } else if warning {
        theme::WARNING
    } else {
        theme::TEXT
    };
    let native_color = if muted_all || warning {
        if warning {
            theme::WARNING
        } else {
            theme::TEXT_MUTED
        }
    } else {
        theme::TEXT_MUTED
    };
    let mut value = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .justify_end()
        .text_align(gpui::TextAlign::Right)
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(1.0))
        .whitespace_normal();
    if let Some(usd) = usd {
        value = value
            .child(
                div()
                    .text_color(rgb(usd_color))
                    .child(SharedString::from(format!(
                        "≈ {}",
                        format_usd_micro_value(usd)
                    ))),
            )
            .child(
                div()
                    .text_color(rgb(native_color))
                    .child(SharedString::from(format!(" · {native}"))),
            );
    } else {
        value = value.child(
            div()
                .text_color(rgb(usd_color))
                .child(SharedString::from(native)),
        );
    }
    value
}

pub(super) fn parse_display_chain_id(value: &str) -> u64 {
    parse_caip2_chain_id(value).unwrap_or_default()
}

fn walletconnect_calldata_context(transaction: &WalletConnectEvmTransaction) -> String {
    let Some(data) = transaction.data.as_deref() else {
        return "None supplied".to_owned();
    };
    let data = data.strip_prefix("0x").unwrap_or(data);
    alloy::hex::decode(data).map_or_else(
        |_| "Supplied (invalid encoding)".to_owned(),
        |bytes| format!("{} bytes supplied", bytes.len()),
    )
}
