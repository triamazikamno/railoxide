use super::*;

pub(super) fn render_staking_action_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    content_width: gpui::Pixels,
    cx: &App,
) -> Option<gpui::Div> {
    let selection = wallet.governance.action_flow.staking_selection()?;
    let selection_ready = wallet.governance.staking.action_selection_ready(selection);
    if let StakingActionKind::RewardClaimSelected { tokens } = &selection.kind {
        return Some(render_reward_claim_selection_form(
            root,
            wallet,
            selection,
            tokens,
            content_width,
            cx,
        ));
    }
    if !selection.kind.is_compose_action() {
        let pending = wallet.governance.action_flow.pending;
        let error = wallet.governance.action_flow.error.as_ref();
        let cancel_root = root.clone();
        let retry_root = root.clone();
        let status = if pending || error.is_none() {
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(8.0))
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text(if selection.kind.is_reward_action() {
                        "Refreshing rewards and estimating the exact call…"
                    } else {
                        "Checking the stake and estimating the exact call…"
                    })
                    .text_size(px(12.0)),
                )
        } else {
            div().w_full().flex().flex_col().gap_3().child(
                Alert::error(
                    "governance-staking-action-error",
                    error.map_or_else(String::new, ToString::to_string),
                )
                .small(),
            )
        };
        return Some(
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(status)
                .child(
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
                        .when(!pending && error.is_some(), |this| {
                            this.child(
                                app_button_base("governance-staking-action-retry")
                                    .small()
                                    .child("Try again")
                                    .on_click(move |_event, window, cx| {
                                        retry_root.update(cx, |root, cx| {
                                            root.review_staking_action(window, cx);
                                        });
                                    }),
                            )
                        }),
                ),
        );
    }

    let amount_input = &wallet.governance.proposal_action_amount_input;
    let delegate_input = &wallet.governance.staking_delegate_input;
    let account = wallet
        .governance_participants()
        .into_iter()
        .find(|account| {
            account.public_account_uuid == selection.actor_uuid
                && account.address == selection.actor
        });
    let actor_label = account
        .as_ref()
        .and_then(public_account_display_label)
        .unwrap_or_else(|| "Public account".to_owned());
    let chain_id = wallet.last_evm_chain;
    let token = governance_contracts(chain_id)
        .map_or(Address::ZERO, |contracts| contracts.governance_token);
    let decimals = governance_contracts(chain_id)
        .and_then(|contracts| {
            wallet
                .effective_token_registry
                .get(chain_id, &contracts.governance_token)
        })
        .map(|token| token.decimals);
    let amount = wallet_ops::parse_send_amount(amount_input.read(cx).value().as_ref(), decimals)
        .ok()
        .filter(|amount| !amount.is_zero());
    let cached_balance = wallet
        .governance
        .staking
        .accounts
        .get(&selection.actor_uuid)
        .and_then(|view| view.balance.as_ref().ok())
        .copied();
    let balance_display = cached_balance.map_or_else(
        || "Balance unavailable".to_owned(),
        |balance| format_staking_amount(chain_id, token, balance, &wallet.effective_token_registry),
    );
    let review_root = root.clone();
    let cancel_root = root.clone();
    let mut content = div().w(content_width).flex().flex_col().gap_3();
    match selection.kind {
        StakingActionKind::Stake => {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_strong_text(actor_label))
                            .child(
                                app_muted_text(short_address(&selection.actor))
                                    .font_family(APP_MONO_FONT_FAMILY),
                            ),
                    )
                    .child(app_muted_text(balance_display)),
            );
            let max_root = root.clone();
            let max_input = amount_input.clone();
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().flex().items_center().justify_end().when_some(
                        cached_balance,
                        |this, balance| {
                            this.child(
                                app_button_base("governance-staking-max")
                                    .link()
                                    .xsmall()
                                    .compact()
                                    .child(format!(
                                        "Max: {} RAIL",
                                        railgun_ui::group_thousands(&format_send_amount_input(
                                            balance, decimals
                                        ))
                                    ))
                                    .on_click(move |_event, window, cx| {
                                        max_input.update(cx, |input, cx| {
                                            input.set_value(
                                                format_send_amount_input(balance, decimals),
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        });
                                        max_root.update(cx, |_, cx| cx.notify());
                                    }),
                            )
                        },
                    ))
                    .child(
                        app_input(amount_input)
                            .small()
                            .w_full()
                            .suffix(app_muted_text("RAIL").text_size(px(11.0))),
                    ),
            );
        }
        StakingActionKind::Delegate { stake_id } => {
            let position = wallet
                .governance
                .staking
                .accounts
                .get(&selection.actor_uuid)
                .and_then(|account| account.stakes.as_ref().ok())
                .and_then(|stakes| stakes.iter().find(|stake| stake.id == stake_id));
            let current_delegate = position.map_or(Address::ZERO, |position| position.delegate);
            let delegation_label = if current_delegate == selection.actor {
                "self-delegated".to_owned()
            } else {
                format!("delegated to {}", short_address(&current_delegate))
            };
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                    .child(app_strong_text(format!(
                        "Stake #{} · {}",
                        stake_id,
                        position.map_or_else(
                            || "Unknown amount".to_owned(),
                            |position| format_staking_amount(
                                chain_id,
                                token,
                                position.amount,
                                &wallet.effective_token_registry
                            )
                        )
                    )))
                    .child(app_muted_text(format!(
                        "{} {} · {}",
                        actor_label,
                        short_address(&selection.actor),
                        delegation_label
                    ))),
            );
            let parsed_delegate = Address::from_str(delegate_input.read(cx).value().trim())
                .ok()
                .filter(|address| *address != Address::ZERO);
            let delegate_error = if !delegate_input.read(cx).value().trim().is_empty()
                && parsed_delegate.is_none()
            {
                Some("Enter a valid Ethereum address")
            } else if parsed_delegate == Some(current_delegate) {
                Some("This stake is already delegated to that address.")
            } else {
                None
            };
            content = content.child(app_input(delegate_input).small().w_full());
            if let Some(error) = delegate_error {
                content = content.child(
                    app_muted_text(error)
                        .text_color(rgb(theme::DANGER))
                        .text_size(px(11.0)),
                );
            }
            content = content.child(
                app_muted_text(
                    "The delegate gets the voting power and rewards. The stake remains yours.",
                )
                .text_size(px(11.0))
                .whitespace_normal(),
            );
        }
        _ => return None,
    }
    let delegate_ready = if let StakingActionKind::Delegate { .. } = selection.kind {
        let position = wallet
            .governance
            .staking
            .accounts
            .get(&selection.actor_uuid)
            .and_then(|account| account.stakes.as_ref().ok())
            .and_then(|stakes| {
                selection
                    .kind
                    .stake_id()
                    .and_then(|id| stakes.iter().find(|stake| stake.id == id))
            });
        let current = position.map(|position| position.delegate);
        Address::from_str(delegate_input.read(cx).value().trim())
            .ok()
            .filter(|address| *address != Address::ZERO)
            .is_some_and(|address| Some(address) != current)
    } else {
        amount.is_some()
    };
    let ready = selection_ready && delegate_ready && !wallet.governance.action_flow.pending;
    let prepare = move |window: &mut Window, cx: &mut App| {
        review_root.update(cx, |root, cx| root.review_staking_action(window, cx));
    };
    let prepare_on_enter = prepare.clone();
    content = content.on_action(move |_: &gpui_component::dialog::Confirm, window, cx| {
        cx.stop_propagation();
        if ready {
            prepare_on_enter(window, cx);
        }
    });
    content = content.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                app_button_base("governance-staking-cancel")
                    .ghost()
                    .small()
                    .child("Cancel")
                    .on_click(move |_event, window, cx| {
                        cancel_root.update(cx, WalletRoot::close_staking_action);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button_base("governance-staking-review")
                    .primary()
                    .small()
                    .loading(wallet.governance.action_flow.pending)
                    .disabled(!ready)
                    .child(if wallet.governance.action_flow.pending {
                        "Preparing authorization…"
                    } else {
                        "Prepare authorization"
                    })
                    .on_click(move |_event, window, cx| prepare(window, cx)),
            ),
    );
    if let Some(error) = wallet.governance.action_flow.error.as_ref() {
        content = content
            .child(Alert::error("governance-staking-action-error", error.to_string()).small());
    }
    Some(content)
}
