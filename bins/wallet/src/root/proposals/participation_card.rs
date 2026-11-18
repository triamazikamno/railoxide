#[allow(clippy::wildcard_imports)]
use super::*;

fn render_proposal_participant_account_column(account: &PublicAccountMetadata) -> gpui::Div {
    let label =
        crate::root::public_account::public_account_display_label(account).unwrap_or_else(|| {
            crate::root::spend_authorization::spend_authorization_recipient_display(&format!(
                "{:#x}",
                account.address
            ))
        });
    let mut account_column = div()
        .flex()
        .flex_col()
        .gap_1()
        .w(px(250.0))
        .flex_none()
        .min_w(px(0.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(app_strong_text(label))
                .child(
                    app_muted_text(
                        crate::root::spend_authorization::spend_authorization_recipient_display(
                            &format!("{:#x}", account.address),
                        ),
                    )
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(px(11.0))
                    .truncate(),
                ),
        );
    if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
        account_column = account_column.child(
            app_muted_text("Inactive account")
                .text_size(px(10.0))
                .text_color(rgb(theme::WARNING)),
        );
    }
    account_column
}

pub(super) fn render_proposal_participation_card(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    proposal: &ResolvedProposal,
    stage: GovernanceProposalStage,
    cx: &App,
) -> gpui::Div {
    let history_kind = proposal_closed_history_kind(stage);
    let actionable = matches!(
        stage,
        GovernanceProposalStage::AwaitingSponsorship
            | GovernanceProposalStage::ReadyToCallVote
            | GovernanceProposalStage::VotingOpen
            | GovernanceProposalStage::NayOnlyVoting
    );
    let key_matches = wallet
        .governance
        .proposal_participation
        .key
        .as_ref()
        .is_some_and(|key| {
            key.index == proposal.proposal.index
                && key.contract == proposal.proposal.contract_address
                && key.version == proposal.proposal.contract_version
        });
    let capacity_kind = proposal_capacity_kind(stage);
    let power_phrase = match capacity_kind {
        ProposalCapacityKind::Sponsorship => "sponsorship",
        ProposalCapacityKind::Voting => "voting",
    };
    let expanded = wallet.proposals.selected.as_ref().is_some_and(|selection| {
        selection.identity == proposal.identity() && selection.participation_expanded
    });
    let participants = wallet.governance_participants();
    let total_accounts = participants.len();
    let mut aggregate_available = Some(U256::ZERO);
    let mut aggregate_maximum = Some(U256::ZERO);
    let mut aggregate_valid = key_matches || participants.is_empty();
    let mut total_used_accounts = 0usize;
    let mut inactive_count = 0usize;
    let mut loading_count = 0usize;
    let mut unavailable_count = 0usize;
    let mut not_loaded_count = 0usize;
    let mut closed_sponsored = Some(U256::ZERO);
    let mut closed_voted = Some(U256::ZERO);
    for account in &participants {
        let row = wallet
            .governance
            .proposal_participation
            .row(&proposal.proposal, &account.address);
        if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
            inactive_count += 1;
        }
        if !key_matches {
            continue;
        }
        match row {
            Some(ProposalParticipationRow::Loading) => loading_count += 1,
            None => not_loaded_count += 1,
            Some(ProposalParticipationRow::Unavailable(_)) => unavailable_count += 1,
            Some(ProposalParticipationRow::Ready(participation)) => {
                let participation_matches = participation.proposal_version
                    == proposal.proposal.contract_version
                    && participation.proposal_id == proposal.proposal.index
                    && participation.voting_contract == proposal.proposal.contract_address;
                if !participation_matches {
                    unavailable_count += 1;
                    continue;
                }
                if let Some(kind) = history_kind {
                    let sponsored = closed_sponsored
                        .and_then(|total| total.checked_add(participation.sponsored));
                    let voted =
                        closed_voted.and_then(|total| total.checked_add(participation.voted));
                    if sponsored.is_none() || voted.is_none() {
                        if aggregate_valid {
                            unavailable_count += 1;
                        }
                        aggregate_valid = false;
                    }
                    closed_sponsored = sponsored;
                    closed_voted = voted;
                    if matches!(kind, ProposalClosedHistoryKind::Voting) {
                        if !participation.voted.is_zero() {
                            total_used_accounts += 1;
                        }
                    } else if !participation.sponsored.is_zero() {
                        total_used_accounts += 1;
                    }
                    continue;
                }
                let sponsor_capacity = participation.sponsorship_capacity();
                let voting_capacity = participation.voting_capacity();
                let capacity = match capacity_kind {
                    ProposalCapacityKind::Sponsorship => &sponsor_capacity,
                    ProposalCapacityKind::Voting => &voting_capacity,
                };
                let Ok(capacity) = capacity else {
                    unavailable_count += 1;
                    aggregate_valid = false;
                    continue;
                };
                let Some((available, maximum)) = capacity.remaining.zip(capacity.snapshot_power)
                else {
                    unavailable_count += 1;
                    aggregate_valid = false;
                    continue;
                };
                aggregate_available =
                    aggregate_available.and_then(|total| total.checked_add(available));
                aggregate_maximum = aggregate_maximum.and_then(|total| total.checked_add(maximum));
                if aggregate_available.is_none() || aggregate_maximum.is_none() {
                    if aggregate_valid {
                        unavailable_count += 1;
                    }
                    aggregate_valid = false;
                }
                if !capacity.allocated.is_zero() {
                    total_used_accounts += 1;
                }
            }
        }
    }
    if !key_matches && total_accounts > 0 {
        loading_count = total_accounts;
    }
    let mut details = div().w_full().flex().flex_col().mt(px(8.0));
    if participants.is_empty() {
        details = details.child(
            app_muted_text("Enroll a Public account to view voting power and allocations.")
                .mb(px(8.0)),
        );
    }
    for account in &participants {
        let row = wallet
            .governance
            .proposal_participation
            .row(&proposal.proposal, &account.address);
        let mut row_card = div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_3()
            .py(px(9.0))
            .px(px(2.0))
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE));
        row_card = row_card.child(render_proposal_participant_account_column(account));
        let mut capacity_column = div().flex().flex_col().gap_1().flex_1().min_w(px(220.0));
        let mut actions = div().flex().flex_wrap().gap_2().flex_none();
        match row {
            Some(ProposalParticipationRow::Ready(participation)) => {
                let participation_matches = participation.proposal_version
                    == proposal.proposal.contract_version
                    && participation.proposal_id == proposal.proposal.index
                    && participation.voting_contract == proposal.proposal.contract_address;
                if participation_matches {
                    if let Some(history_kind) = history_kind {
                        let allocation = match history_kind {
                            ProposalClosedHistoryKind::Sponsorship => {
                                if participation.sponsored.is_zero() {
                                    "No sponsorship allocated".to_owned()
                                } else {
                                    format!(
                                        "{} sponsored",
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                }
                            }
                            ProposalClosedHistoryKind::Voting => {
                                if participation.voted.is_zero()
                                    && participation.sponsored.is_zero()
                                {
                                    "No allocation".to_owned()
                                } else if participation.voted.is_zero() {
                                    format!(
                                        "{} sponsored",
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                } else if participation.sponsored.is_zero() {
                                    format!(
                                        "{} voted",
                                        format_compact_rail_amount_with_unit(participation.voted)
                                    )
                                } else {
                                    format!(
                                        "{} voted · {} sponsored",
                                        format_compact_rail_amount_with_unit(participation.voted),
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                }
                            }
                        };
                        capacity_column =
                            capacity_column.child(app_text(allocation).text_size(px(12.0)));
                    } else {
                        let capacity_result = match capacity_kind {
                            ProposalCapacityKind::Sponsorship => {
                                participation.sponsorship_capacity()
                            }
                            ProposalCapacityKind::Voting => participation.voting_capacity(),
                        };
                        match capacity_result {
                            Ok(capacity) => {
                                let remaining = capacity.remaining.unwrap_or_default();
                                let snapshot = capacity.snapshot_power.unwrap_or_default();
                                let kind = match capacity_kind {
                                    ProposalCapacityKind::Sponsorship => "sponsor",
                                    ProposalCapacityKind::Voting => "vote",
                                };
                                if capacity.allocated.is_zero() {
                                    capacity_column = capacity_column.child(
                                        app_text(format!(
                                            "{} available to {kind}",
                                            format_compact_rail_amount_with_unit(snapshot)
                                        ))
                                        .text_size(px(12.0)),
                                    );
                                } else {
                                    capacity_column = capacity_column.child(
                                        app_text(format!(
                                            "{} of {} RAIL",
                                            format_compact_rail_amount(remaining),
                                            format_compact_rail_amount(snapshot)
                                        ))
                                        .text_size(px(12.0)),
                                    );
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "available · {} {}",
                                            format_compact_rail_amount_with_unit(
                                                capacity.allocated
                                            ),
                                            if capacity_kind == ProposalCapacityKind::Sponsorship {
                                                "sponsored"
                                            } else {
                                                "voted"
                                            }
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                    capacity_column = capacity_column.child(
                                        Progress::new(SharedString::from(format!(
                                            "proposal-account-capacity-{}",
                                            account.public_account_uuid
                                        )))
                                        .w_full()
                                        .max_w(px(340.0))
                                        .value(f32::from(per_mille(remaining, snapshot)) / 10.0),
                                    );
                                    if remaining.is_zero() {
                                        capacity_column = capacity_column.child(
                                            app_muted_text(format!(
                                                "All {power_phrase} power used for this proposal"
                                            ))
                                            .text_size(px(11.0))
                                            .text_color(rgb(theme::WARNING)),
                                        );
                                    }
                                }
                                if capacity_kind == ProposalCapacityKind::Voting
                                    && !participation.sponsored.is_zero()
                                {
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "Sponsored {} earlier in this proposal",
                                            format_compact_rail_amount_with_unit(
                                                participation.sponsored
                                            )
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                }
                                let active_snapshot = match capacity_kind {
                                    ProposalCapacityKind::Sponsorship => {
                                        participation.sponsorship_snapshot.voting_power
                                    }
                                    ProposalCapacityKind::Voting => {
                                        participation.voting_snapshot.voting_power
                                    }
                                };
                                if participation.current_voting_power != active_snapshot {
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "Live received power {} (snapshot {})",
                                            format_compact_rail_amount_with_unit(
                                                participation.current_voting_power
                                            ),
                                            format_compact_rail_amount_with_unit(active_snapshot)
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                }
                            }
                            Err(error) => {
                                capacity_column = capacity_column.child(
                                    Alert::error(
                                        SharedString::from(format!(
                                            "governance-capacity-error-{}",
                                            account.address
                                        )),
                                        format!("Capacity unavailable: {error}"),
                                    )
                                    .small(),
                                );
                            }
                        }
                    }
                } else {
                    capacity_column = capacity_column.child(app_muted_text("Participation data does not match this proposal; actions are unavailable.").text_size(px(11.0)));
                }
                if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
                    capacity_column = capacity_column.child(
                        app_muted_text(
                            "Inactive account, shown for information; actions disabled.",
                        )
                        .text_size(px(11.0))
                        .text_color(rgb(theme::WARNING)),
                    );
                }
                let sponsor_remaining = participation
                    .sponsorship_capacity()
                    .ok()
                    .and_then(|capacity| capacity.remaining);
                let voting_remaining = participation
                    .voting_capacity()
                    .ok()
                    .and_then(|capacity| capacity.remaining);
                if key_matches
                    && participation_matches
                    && actionable
                    && account.status == wallet_ops::vault::PublicAccountStatus::Active
                {
                    let actor = account.address;
                    let open = |kind: ProposalActionKind, root: &Entity<WalletRoot>| {
                        let root = root.clone();
                        let mut button = app_button_base(SharedString::from(format!(
                            "governance-proposal-action-{kind:?}-{actor}"
                        )))
                        .child(match kind {
                            ProposalActionKind::Sponsor => "Sponsor",
                            ProposalActionKind::Unsponsor => "Unsponsor",
                            ProposalActionKind::CallVote => "Call vote",
                            ProposalActionKind::Yay => "Yay",
                            ProposalActionKind::Nay => "Nay",
                        });
                        button = match kind {
                            ProposalActionKind::Yay => {
                                button.small().primary().icon(IconName::ThumbsUp)
                            }
                            ProposalActionKind::Nay => {
                                button.small().warning().icon(IconName::ThumbsDown)
                            }
                            ProposalActionKind::Sponsor
                            | ProposalActionKind::Unsponsor
                            | ProposalActionKind::CallVote => button.outline().small(),
                        };
                        button.on_click(move |_event, window, cx| {
                            root.update(cx, |root, cx| {
                                root.open_proposal_action(actor, kind, window, cx);
                            });
                        })
                    };
                    match stage {
                        GovernanceProposalStage::AwaitingSponsorship => {
                            if sponsor_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions.child(open(ProposalActionKind::Sponsor, root));
                            }
                            if !participation.sponsored.is_zero() {
                                actions = actions.child(open(ProposalActionKind::Unsponsor, root));
                            }
                        }
                        GovernanceProposalStage::ReadyToCallVote => {
                            if sponsor_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions.child(open(ProposalActionKind::Sponsor, root));
                            }
                            if !participation.sponsored.is_zero() {
                                actions = actions.child(open(ProposalActionKind::Unsponsor, root));
                            }
                            actions = actions.child(open(ProposalActionKind::CallVote, root));
                        }
                        GovernanceProposalStage::VotingOpen => {
                            if voting_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions
                                    .child(open(ProposalActionKind::Yay, root))
                                    .child(open(ProposalActionKind::Nay, root));
                            }
                        }
                        GovernanceProposalStage::NayOnlyVoting
                            if voting_remaining.is_some_and(|remaining| !remaining.is_zero()) =>
                        {
                            actions = actions.child(open(ProposalActionKind::Nay, root));
                        }
                        _ => {}
                    }
                }
            }
            Some(ProposalParticipationRow::Unavailable(error)) => {
                capacity_column = capacity_column
                    .child(
                        Alert::error(
                            SharedString::from(format!(
                                "governance-participation-error-{}",
                                account.address
                            )),
                            error.to_string(),
                        )
                        .small(),
                    )
                    .child(
                        app_muted_text("Actions unavailable until this account refreshes.")
                            .text_size(px(11.0)),
                    );
            }
            Some(ProposalParticipationRow::Loading) => {
                capacity_column = capacity_column.child(
                    app_muted_shimmer_text("Loading account participation…").text_size(px(11.0)),
                );
            }
            None => {
                capacity_column = capacity_column
                    .child(app_muted_text("Participation not loaded").text_size(px(11.0)));
            }
        }
        row_card = row_card.child(capacity_column).child(actions);
        details = details.child(row_card);
    }
    if !key_matches {
        details = details.child(
            app_muted_shimmer_text("Refreshing participation for this proposal…")
                .text_size(px(11.0))
                .mt(px(8.0)),
        );
    }
    let summary_state = if !key_matches && total_accounts > 0 {
        ProposalCapacitySummaryState::Loading
    } else if !aggregate_valid {
        ProposalCapacitySummaryState::Unavailable
    } else if not_loaded_count > 0 && loading_count == 0 && unavailable_count == 0 {
        ProposalCapacitySummaryState::NotLoaded
    } else {
        proposal_capacity_summary_state(
            aggregate_available,
            aggregate_maximum,
            loading_count,
            unavailable_count,
            total_accounts,
        )
    };
    let (kind_phrase, action_word) = match capacity_kind {
        ProposalCapacityKind::Sponsorship => ("sponsor", "sponsored"),
        ProposalCapacityKind::Voting => ("vote", "voted"),
    };
    let account_word = if total_accounts == 1 {
        "account"
    } else {
        "accounts"
    };
    let mut summary = div().w_full().flex().flex_col().gap_2();
    match (summary_state, history_kind) {
        (ProposalCapacitySummaryState::Empty, _) => {
            summary = summary.child(
                app_muted_text(
                    "No participating accounts enrolled · enroll an account to view governance power",
                )
                .text_size(px(12.0)),
            );
        }
        (ProposalCapacitySummaryState::Loading, _) => {
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .py(px(6.0))
                    .child(Spinner::new().small())
                    .child(
                        app_muted_shimmer_text("Loading account participation…")
                            .text_size(px(12.0)),
                    ),
            );
        }
        (ProposalCapacitySummaryState::NotLoaded, _) => {
            summary = summary
                .child(app_muted_text("Account participation not loaded").text_size(px(12.0)));
        }
        (ProposalCapacitySummaryState::Unavailable, _) => {
            let unavailable_word = if unavailable_count == 1 {
                "account unavailable"
            } else {
                "accounts unavailable"
            };
            let inactive_suffix = if inactive_count == 0 {
                String::new()
            } else {
                format!(" · {inactive_count} inactive")
            };
            summary = summary.child(div().w_full().flex().items_baseline().flex_wrap().gap_2().child(app_text("—").text_size(px(16.0)).text_color(rgb(theme::DANGER))).child(app_muted_text("available power unknown").text_size(px(12.0))).child(div().ml_auto().child(app_text(format!("{unavailable_count} {unavailable_word} · expand for details{inactive_suffix}")).text_size(px(12.0)).text_color(rgb(theme::DANGER)))));
        }
        (
            ProposalCapacitySummaryState::Full
            | ProposalCapacitySummaryState::Partial
            | ProposalCapacitySummaryState::Exhausted,
            Some(history),
        ) => {
            let sponsored = closed_sponsored.unwrap_or_default();
            let voted = closed_voted.unwrap_or_default();
            let (history_text, history_amount) = match history {
                ProposalClosedHistoryKind::Sponsorship => {
                    ("Sponsorship closed · your accounts allocated", sponsored)
                }
                ProposalClosedHistoryKind::Voting => {
                    ("Voting closed · your accounts allocated", voted)
                }
            };
            let right = if total_accounts == 0 {
                "0 accounts · enroll an account to view history".to_owned()
            } else if inactive_count > 0 {
                format!(
                    "{total_accounts} {account_word} · {total_used_accounts} {action_word} · {inactive_count} inactive"
                )
            } else {
                format!("{total_accounts} {account_word} · {total_used_accounts} {action_word}")
            };
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_baseline()
                    .flex_wrap()
                    .gap_2()
                    .child(app_muted_text(history_text).text_size(px(12.0)).mr(px(8.0)))
                    .child(
                        app_text(format_compact_rail_amount_with_unit(history_amount))
                            .text_size(px(16.0))
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .child(app_muted_text(right).text_size(px(12.0))),
                    ),
            );
        }
        (state, None) => {
            let available = aggregate_available.unwrap_or_default();
            let maximum = aggregate_maximum.unwrap_or_default();
            let amount = if state == ProposalCapacitySummaryState::Full {
                format_compact_rail_amount_with_unit(available)
            } else {
                format!(
                    "{} of {} RAIL",
                    format_compact_rail_amount(available),
                    format_compact_rail_amount(maximum)
                )
            };
            let right = if total_accounts == 0 {
                "0 accounts · enroll an account to participate".to_owned()
            } else if state == ProposalCapacitySummaryState::Exhausted {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · all {power_phrase} power is used · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · all {power_phrase} power is used")
                }
            } else if total_used_accounts == 0 {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · none {action_word} yet · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · none {action_word} yet")
                }
            } else {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · {total_used_accounts} {action_word} · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · {total_used_accounts} {action_word}")
                }
            };
            let amount_text = app_strong_text(amount).text_size(px(16.0));
            let amount_text = if state == ProposalCapacitySummaryState::Exhausted {
                amount_text.text_color(rgb(theme::WARNING))
            } else {
                amount_text
            };
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_baseline()
                    .flex_wrap()
                    .gap_2()
                    .child(amount_text)
                    .child(
                        app_muted_text(format!("available to {kind_phrase}"))
                            .text_size(px(12.0))
                            .ml(px(8.0)),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .child(app_muted_text(right).text_size(px(12.0))),
                    ),
            );
            if matches!(
                state,
                ProposalCapacitySummaryState::Partial | ProposalCapacitySummaryState::Exhausted
            ) {
                summary = summary.child(
                    Progress::new("proposal-capacity-summary")
                        .w_full()
                        .value(f32::from(per_mille(available, maximum)) / 10.0),
                );
            }
        }
    }
    if key_matches && total_accounts > 0 && loading_count == 0 {
        let refresh = wallet
            .governance
            .proposal_participation
            .unloaded(&proposal.proposal)
            .is_empty();
        let request_root = root.clone();
        summary = summary.child(
            div().w_full().flex().justify_end().child(
                app_button_base(SharedString::from(format!(
                    "wallet-proposal-participation-request-{}-{}-{}",
                    proposal_version_label(proposal.proposal.contract_version),
                    proposal.proposal.contract_address,
                    proposal.proposal.index,
                )))
                .outline()
                .small()
                .child(if refresh {
                    "Refresh participation"
                } else {
                    "Show participation"
                })
                .on_click(move |_event, _window, cx| {
                    request_root.update(cx, |root, cx| {
                        root.request_proposal_participation(refresh, cx);
                    });
                }),
            ),
        );
    }
    let toggle_root = root.clone();
    let toggle_id = SharedString::from(format!(
        "wallet-proposal-participation-toggle-{}-{}-{}",
        proposal_version_label(proposal.proposal.contract_version),
        proposal.proposal.contract_address,
        proposal.proposal.index,
    ));
    let card = proposal_card("Participation").child(
        Collapsible::new()
            .open(expanded)
            .w_full()
            .child(summary)
            .content(details),
    );
    div().relative().w_full().pb(px(10.0)).child(card).child(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(1.0))
            .flex()
            .justify_center()
            .child(render_proposal_participation_toggle(
                toggle_root,
                toggle_id,
                expanded,
                cx,
            )),
    )
}

fn render_proposal_participation_toggle(
    toggle_root: Entity<WalletRoot>,
    toggle_id: SharedString,
    expanded: bool,
    cx: &App,
) -> gpui::AnyElement {
    app_button_base(toggle_id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(rgb(theme::SURFACE).into())
                .foreground(rgb(theme::TEXT).into())
                .hover(rgb(theme::SURFACE_HOVER_SUBTLE).into())
                .active(rgb(theme::SURFACE_HOVER).into()),
        )
        .outline()
        .bg(rgb(theme::SURFACE))
        .border_color(rgb(theme::BORDER))
        .xsmall()
        .rounded_full()
        .icon(Icon::new(if expanded {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        }))
        .accessibility_label(if expanded {
            "Hide participation details"
        } else {
            "Show participation details"
        })
        .tooltip(if expanded {
            "Hide participation details"
        } else {
            "Show participation details"
        })
        .on_click(move |_event, _window, cx| {
            toggle_root.update(cx, |root, cx| {
                root.toggle_proposal_participation(cx);
            });
        })
        .into_any_element()
}

pub(in crate::root) const fn proposal_action_title(kind: ProposalActionKind) -> &'static str {
    match kind {
        ProposalActionKind::Sponsor => "Sponsor proposal",
        ProposalActionKind::Unsponsor => "Withdraw sponsorship",
        ProposalActionKind::CallVote => "Call vote",
        ProposalActionKind::Yay => "Vote Yay",
        ProposalActionKind::Nay => "Vote Nay",
    }
}
