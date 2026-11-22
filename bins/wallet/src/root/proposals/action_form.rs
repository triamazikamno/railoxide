use super::*;

pub(super) fn render_proposal_action_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    proposal: &ResolvedProposal,
    decimals: Option<u8>,
    content_width: gpui::Pixels,
    chain_time: U256,
    cx: &App,
) -> Option<gpui::Div> {
    let displayed_key =
        proposal_participation_key(&proposal.proposal, wallet.governance_context_key());
    if !wallet.proposal_participation_key_matches(&proposal.proposal, &displayed_key) {
        return None;
    }
    let Some(crate::root::governance::GovernanceActionSelection::Proposal(selection)) =
        wallet.governance.action_flow.selection.as_ref()
    else {
        return None;
    };
    let Some(ProposalParticipationRow::Ready(participation)) = wallet
        .governance
        .proposal_participation
        .row(&proposal.proposal, &selection.actor)
    else {
        return Some(
            app_muted_text(
                "Selected account participation is unavailable; refresh before authorizing.",
            )
            .text_size(px(11.0)),
        );
    };
    let participation = participation.as_ref();
    if participation.proposal_version != proposal.proposal.contract_version
        || participation.proposal_id != proposal.proposal.index
        || participation.voting_contract != proposal.proposal.contract_address
    {
        return None;
    }
    let amount_input = &wallet.governance.proposal_action_amount_input;
    let raw_amount = amount_input.read(cx).value();
    let raw_amount_present = !raw_amount.trim().is_empty();
    let parsed_amount = wallet_ops::parse_send_amount(raw_amount.as_ref(), decimals);
    let amount = parsed_amount
        .as_ref()
        .ok()
        .filter(|amount| !amount.is_zero())
        .copied();
    let (maximum, capacity_label) = match selection.kind {
        ProposalActionKind::Sponsor => (
            participation
                .sponsorship_capacity()
                .ok()
                .and_then(|capacity| capacity.remaining),
            "available to sponsor",
        ),
        ProposalActionKind::Unsponsor => (Some(participation.sponsored), "sponsored"),
        ProposalActionKind::Yay | ProposalActionKind::Nay => (
            participation
                .voting_capacity()
                .ok()
                .and_then(|capacity| capacity.remaining),
            "available voting power",
        ),
        ProposalActionKind::CallVote => (None, ""),
    };
    let sponsor_lockout_until = matches!(selection.kind, ProposalActionKind::Sponsor)
        .then(|| {
            (participation.last_sponsored.proposal_id != proposal.proposal.index)
                .then_some(participation.last_sponsored.last_sponsor_time)
                .and_then(|last| {
                    last.checked_add(U256::from(
                        wallet_ops::GOVERNANCE_SPONSOR_LOCKOUT_TIME_SECONDS,
                    ))
                })
        })
        .flatten()
        .filter(|until| chain_time <= *until);
    let capacity_text = maximum.map(|maximum| {
        format!(
            "{} RAIL {}",
            format_compact_rail_amount(maximum),
            capacity_label,
        )
    });
    let validation = if sponsor_lockout_until.is_some() {
        Some("This account is temporarily locked from sponsoring another proposal.".to_owned())
    } else if raw_amount_present {
        validate_proposal_action(
            &proposal.proposal,
            &proposal.rules,
            chain_time,
            participation,
            selection.kind,
            amount,
        )
        .err()
        .map(|error| humanize_proposal_action_error(error, selection.kind, maximum))
    } else {
        None
    };
    let parse_error = raw_amount_present
        .then_some(parsed_amount.as_ref().err())
        .flatten()
        .map(|error| format!("Enter a valid RAIL amount ({error})"));
    let validation = parse_error.or(validation);
    let ready = if matches!(selection.kind, ProposalActionKind::CallVote) {
        true
    } else {
        amount.is_some() && validation.is_none() && sponsor_lockout_until.is_none()
    };
    let proposal_for_review = proposal.proposal.clone();
    let selection = *selection;
    let action_pending = wallet.governance.action_flow.pending;
    let close_root = root.clone();
    let max_root = root.clone();
    let review_root = root.clone();
    let prepare = move |window: &mut Window, cx: &mut App| {
        review_root.update(cx, |root, cx| {
            root.review_proposal_action(&proposal_for_review, selection, amount, window, cx);
        });
    };
    let prepare_on_enter = prepare.clone();
    let max_input = amount_input.clone();
    let max_value = maximum.map(|maximum| format_send_amount_input(maximum, decimals));
    let action_label = "Prepare authorization";
    let display_label = public_account_display_label_for_proposal_actor(wallet, selection.actor);
    let mut content = div()
        .when(
            !matches!(selection.kind, ProposalActionKind::CallVote),
            |this| {
                this.on_action(move |_: &gpui_component::dialog::Confirm, window, cx| {
                    cx.stop_propagation();
                    if ready && !action_pending {
                        prepare_on_enter(window, cx);
                    }
                })
            },
        )
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(
            app_muted_text(format!(
                "Proposal #{} · {}",
                proposal.proposal.index,
                proposal
                    .document()
                    .filter(|document| document.available && !document.title.is_empty())
                    .map_or("Governance proposal", |document| document.title.as_str()),
            ))
            .text_size(px(12.0))
            .truncate(),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .p_2()
                .rounded_md()
                .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .min_w(px(0.0))
                        .child(
                            app_strong_text(display_label)
                                .text_size(px(12.0))
                                .line_height(px(16.0)),
                        )
                        .child(
                            app_muted_text(short_address(&selection.actor))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(11.0))
                                .line_height(px(16.0))
                                .truncate(),
                        ),
                )
                .when_some(capacity_text, |this, text| {
                    this.child(
                        app_muted_text(text)
                            .text_size(px(11.0))
                            .line_height(px(16.0))
                            .ml_auto()
                            .truncate(),
                    )
                }),
        );
    if !matches!(selection.kind, ProposalActionKind::CallVote) {
        content = content.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(app_muted_text("Amount").text_size(px(12.0)))
                        .when_some(maximum, |this, maximum| {
                            this.child(
                                app_button_base("governance-action-max")
                                    .link()
                                    .xsmall()
                                    .compact()
                                    .disabled(sponsor_lockout_until.is_some())
                                    .child(format!(
                                        "Max: {} RAIL",
                                        railgun_ui::group_thousands(&format_send_amount_input(
                                            maximum, decimals
                                        ))
                                    ))
                                    .on_click(move |_event, window, cx| {
                                        if let Some(value) = &max_value {
                                            max_input.update(cx, |input, cx| {
                                                input.set_value(value.clone(), window, cx);
                                                cx.notify();
                                            });
                                        }
                                        max_root.update(cx, |_, cx| cx.notify());
                                    }),
                            )
                        }),
                )
                .child(
                    app_input(amount_input)
                        .small()
                        .disabled(sponsor_lockout_until.is_some())
                        .suffix(app_muted_text("RAIL").text_size(px(11.0))),
                )
                .children(validation.clone().map(|error| {
                    app_muted_text(error)
                        .text_color(rgb(theme::DANGER))
                        .text_size(px(11.0))
                }))
                .children(
                    (validation.is_none()
                        && amount.is_some()
                        && matches!(selection.kind, ProposalActionKind::Unsponsor))
                    .then(|| {
                        app_muted_text("Returns to this account's available sponsorship power")
                            .text_size(px(11.0))
                    }),
                )
                .children(
                    (validation.is_none()
                        && amount.is_some()
                        && !matches!(selection.kind, ProposalActionKind::Unsponsor))
                    .then(|| {
                        let remaining = maximum.and_then(|maximum| {
                            amount.and_then(|amount| maximum.checked_sub(amount))
                        });
                        remaining.map(|remaining| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    app_muted_text(format!(
                                        "{} RAIL will remain available",
                                        format_compact_rail_amount(remaining)
                                    ))
                                    .text_size(px(11.0)),
                                )
                                .child(
                                    Progress::new("proposal-action-remaining-capacity")
                                        .w_full()
                                        .value(
                                            maximum.filter(|maximum| !maximum.is_zero()).map_or(
                                                0.0,
                                                |maximum| {
                                                    f32::from(per_mille(remaining, maximum)) / 10.0
                                                },
                                            ),
                                        ),
                                )
                        })
                    })
                    .flatten(),
                ),
        );
    }
    if matches!(selection.kind, ProposalActionKind::Sponsor) {
        if let Some(unlock) = sponsor_lockout_until {
            content = content.child(
                Alert::warning(
                    "governance-action-sponsor-lockout",
                    format!(
                        "Sponsoring another proposal is locked until {}.",
                        format_datetime_short(&unlock)
                    ),
                )
                .small(),
            );
        }
        content = content.child(app_muted_text(
            "Sponsorship can be withdrawn before the vote is called. Sponsoring starts a 7-day lockout on other proposals with this account.",
        ).text_size(px(11.0)));
    }
    if matches!(selection.kind, ProposalActionKind::Unsponsor) {
        let after = proposal
            .proposal
            .sponsorship
            .checked_sub(amount.unwrap_or_default());
        if after.is_some_and(|after| after < proposal.rules.sponsor_threshold) {
            content = content.child(
                Alert::warning(
                    "governance-action-unsponsor-warning",
                    "This withdrawal lowers proposal sponsorship below the threshold required to call a vote.",
                )
                .small(),
            );
        }
    }
    let action_error = wallet
        .governance
        .action_flow
        .error
        .as_ref()
        .map(|error| Alert::error("governance-action-error", error.to_string()).small());
    content = content.children(action_error).child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                app_button_base("governance-action-cancel")
                    .ghost()
                    .small()
                    .child("Cancel")
                    .on_click(move |_event, window, cx| {
                        close_root.update(cx, WalletRoot::close_proposal_action);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button_base("governance-action-review")
                    .primary()
                    .small()
                    .loading(action_pending)
                    .disabled(!ready || action_pending)
                    .child(if action_pending {
                        "Preparing authorization…"
                    } else {
                        action_label
                    })
                    .on_click(move |_event, window, cx| prepare(window, cx)),
            ),
    );
    Some(content)
}

fn public_account_display_label_for_proposal_actor(wallet: &WalletRoot, actor: Address) -> String {
    wallet
        .governance_participants()
        .into_iter()
        .find(|account| account.address == actor)
        .and_then(|account| crate::root::public_account::public_account_display_label(&account))
        .unwrap_or_else(|| "Public account".to_owned())
}

fn humanize_proposal_action_error(
    error: String,
    kind: ProposalActionKind,
    maximum: Option<U256>,
) -> String {
    let amount_limit =
        maximum.map(|maximum| format!(" ({})", format_compact_rail_amount_with_unit(maximum)));
    let limit = amount_limit.as_deref().unwrap_or("");
    match kind {
        ProposalActionKind::Sponsor if error.contains("sponsorship capacity") => {
            format!("Exceeds this account's available sponsorship power{limit}")
        }
        ProposalActionKind::Unsponsor if error.contains("sponsored") => {
            format!("Exceeds this account's sponsored amount{limit}")
        }
        ProposalActionKind::Yay | ProposalActionKind::Nay
            if error.contains("voting capacity") || error.contains("snapshot") =>
        {
            format!("Exceeds this account's available voting power{limit}")
        }
        _ => error,
    }
}
