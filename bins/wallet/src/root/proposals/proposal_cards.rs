use super::*;

fn proposal_copy_control(
    proposal: &ResolvedProposal,
    label: &'static str,
    value: String,
    field: &'static str,
) -> gpui::Div {
    let display_value = spend_authorization_recipient_display(&value);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label).text_size(px(11.0)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mt(px(1.0))
                .child(
                    app_text(display_value)
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(rgb(theme::TEXT))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0)),
                )
                .child(
                    div()
                        .id(proposal_copy_id(proposal, field, "action"))
                        .flex_none()
                        .tooltip(move |window, cx| {
                            Tooltip::new(format!("Copy {label}")).build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            proposal_copy_id(proposal, field, "clipboard"),
                            value,
                        )),
                ),
        )
}

pub(super) fn render_proposal_actions_card(
    root: &Entity<WalletRoot>,
    proposal: &ResolvedProposal,
    chain_id: u64,
    expanded_calldata: &BTreeSet<ProposalActionIdentity>,
    anchor_rates: &TokenAnchorRateCache,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> impl IntoElement {
    let identity = proposal.identity();
    let mut card = div()
        .id(SharedString::from(format!(
            "wallet-proposal-actions-{}-{}-{}",
            proposal_version_label(identity.contract_version),
            identity.contract_address,
            identity.index,
        )))
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    if proposal.proposal.actions.is_empty() {
        return card.child(app_muted_text("No contract actions"));
    }
    for (ordinal, action) in proposal.proposal.actions.iter().enumerate() {
        let action_identity = ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal,
        };
        let calldata = action_calldata_hex(&action.calldata);
        let expanded = expanded_calldata.contains(&action_identity);
        let compact_calldata = compact_calldata_display(&calldata);
        let address = action.call_contract.to_checksum(None);
        let wrapped_native_token = effective_chain.and_then(|chain| chain.wrapped_native_token);
        let railgun_contract = effective_chain.and_then(|chain| {
            chain
                .railgun
                .as_ref()
                .map(|private| private.deployment.contract)
        });
        let decoded = decode_proposal_action(
            chain_id,
            action.call_contract,
            &action.calldata,
            wrapped_native_token,
            railgun_contract,
        );
        let target_label = proposal_action_target_label(
            chain_id,
            action.call_contract,
            token_registry,
            effective_chain,
            decoded.as_ref(),
        );
        let value = action.value;
        let address_copy_id = proposal_action_id(&identity, ordinal, "address", "clipboard");
        let calldata_copy_id = proposal_action_id(&identity, ordinal, "calldata", "clipboard");
        let calldata_toggle_id = proposal_action_id(&identity, ordinal, "calldata", "toggle");
        let calldata_copy_wrapper_id =
            proposal_action_id(&identity, ordinal, "calldata", "copy-wrapper");
        let calldata_toggle_root = root.clone();
        let calldata_toggle_identity = identity.clone();
        let mut action_card = div()
            .id(proposal_action_id(&identity, ordinal, "wrapper", "card"))
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(12.0))
            .rounded_md()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                app_strong_text(match decoded.as_ref() {
                    Some(decoded) => format!("Action {} · {}", ordinal + 1, decoded.method_name()),
                    None => format!("Action {}", ordinal + 1),
                })
                .text_size(px(13.0)),
            )
            .child(app_muted_text("Target").text_size(px(11.0)))
            .child({
                let mut target_row = div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .max_w_full()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                app_text(address.clone())
                                    .max_w_full()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0)),
                            )
                            .child(
                                div()
                                    .id(proposal_action_id(&identity, ordinal, "address", "action"))
                                    .flex_none()
                                    .tooltip(|window, cx| {
                                        Tooltip::new("Copy target").build(window, cx)
                                    })
                                    .child(clipboard_with_toast(address_copy_id, address.clone())),
                            ),
                    );
                if let Some(label) = target_label {
                    target_row = target_row.child(proposal_address_badge(label));
                }
                target_row
            })
            .children(decoded.as_ref().map(|decoded| {
                let preview = proposal_decoded_preview(
                    decoded,
                    action.call_contract,
                    value,
                    chain_id,
                    anchor_rates,
                    token_registry,
                    effective_chain,
                    public_accounts,
                    public_address_book,
                );
                render_proposal_decoded_preview(&preview, &identity, ordinal)
            }))
            .child(app_muted_text("Raw calldata").text_size(px(11.0)))
            .child(
                Collapsible::new()
                    .open(expanded)
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                app_button_base(calldata_toggle_id)
                                    .text()
                                    .small()
                                    .compact()
                                    .icon(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(
                                        app_text(compact_calldata)
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .whitespace_nowrap(),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        cx.stop_propagation();
                                        calldata_toggle_root.update(cx, |root, cx| {
                                            root.toggle_proposal_calldata(
                                                calldata_toggle_identity.clone(),
                                                ordinal,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                    )
                    .content(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .pl(px(14.0))
                            .pt(px(8.0))
                            .pb(px(7.0))
                            .child(
                                div()
                                    .id(proposal_action_id(
                                        &identity, ordinal, "calldata", "expanded",
                                    ))
                                    .w_full()
                                    .min_w(px(0.0))
                                    .relative()
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(-2.0))
                                            .right(px(0.0))
                                            .id(calldata_copy_wrapper_id)
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy calldata").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                calldata_copy_id,
                                                calldata.clone(),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .min_w(px(0.0))
                                            .pr(px(28.0))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .line_height(px(16.0))
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .whitespace_normal()
                                            .child(SharedString::from(calldata)),
                                    ),
                            ),
                    ),
            );
        if !value.is_zero() {
            action_card = action_card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_muted_text("Value").text_size(px(11.0)))
                    .child(
                        app_text(format_native_token_amount_for_display(chain_id, value))
                            .text_size(px(12.0))
                            .font_family(APP_MONO_FONT_FAMILY),
                    ),
            );
        }
        card = card.child(action_card);
    }
    card
}

pub(super) fn proposal_document_card(
    proposal: &ResolvedProposal,
    description_id: SharedString,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    _window: &mut Window,
    _cx: &mut Context<'_, WalletRoot>,
) -> gpui::Div {
    let mut card = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    let mut header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(app_strong_text("Proposal").text_size(px(13.0)))
        .child(div().flex_1());
    if let Some(document) = proposal.document().filter(|document| document.available) {
        header = header.child(
            div()
                .id(proposal_copy_id(proposal, "proposal", "action"))
                .tooltip(|window, cx| Tooltip::new("Copy proposal text").build(window, cx))
                .child(clipboard_with_toast(
                    proposal_copy_id(proposal, "proposal", "clipboard"),
                    document.description.clone(),
                )),
        );
    }
    let content = match proposal.document() {
        None => div()
            .flex()
            .flex_col()
            .gap_2()
            .children([520.0_f32, 460.0, 500.0, 340.0].into_iter().map(|width| {
                Skeleton::new()
                    .secondary()
                    .h(px(14.0))
                    .w(px(width))
                    .max_w_full()
            }))
            .into_any_element(),
        Some(document) if !document.available => div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .p(px(24.0))
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_6()
                    .text_color(rgb(theme::TEXT_MUTED)),
            )
            .child(app_muted_text(
                "Document could not be retrieved from IPFS gateways.",
            ))
            .child(app_muted_text("Refresh retries the download.").text_size(px(12.0)))
            .into_any_element(),
        Some(document) => match proposal.presentation() {
            Some(ProposalPresentation::Prepared(prepared)) => render_proposal_blocks(
                &prepared.blocks,
                &description_id,
                "root",
                &proposal.identity(),
                table_scroll_handles,
            ),
            Some(ProposalPresentation::RawParseFallback(source)) => {
                TextView::markdown(description_id, source.clone())
                    .selectable(true)
                    .into_any_element()
            }
            None => TextView::markdown(
                description_id,
                inert_raw_fallback_source(&document.description),
            )
            .selectable(true)
            .into_any_element(),
            Some(ProposalPresentation::TooComplex) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .p(px(24.0))
                .child(app_muted_text(
                    "This proposal is too complex to render safely.",
                ))
                .child(
                    app_muted_text("Copy the proposal text to view it externally.")
                        .text_size(px(12.0)),
                )
                .into_any_element(),
        },
    };
    card = card.child(header).child(content);
    card
}
pub(super) fn proposal_card(title: &'static str) -> gpui::Div {
    div()
        .w_full()
        .p(px(14.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .gap_2()
        .child(app_strong_text(title).text_size(px(13.0)))
}
pub(super) fn sponsorship_per_mille(proposal: &ResolvedProposal) -> u16 {
    let scale = U256::from(10).pow(U256::from(18));
    let threshold = u128::try_from(proposal.rules.sponsor_threshold / scale).unwrap_or(0);
    let sponsored = u128::try_from(proposal.proposal.sponsorship / scale).unwrap_or(0);
    per_mille(U256::from(sponsored), U256::from(threshold))
}
pub(super) fn render_sponsorship_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let per_mille = sponsorship_per_mille(proposal);
    let fraction = ratio_fraction(per_mille);
    let color = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        theme::TEXT_MUTED
    } else {
        theme::PRIMARY
    };
    let percent = (u32::from(per_mille) + 5) / 10;
    let mut footer_row = div()
        .flex()
        .justify_between()
        .child(app_muted_text(format!("{percent}% of threshold")).text_size(px(11.0)));
    if status.stage == GovernanceProposalStage::SponsorshipExpired {
        footer_row = footer_row.child(
            app_muted_text(format!(
                "Expired {}",
                format_deadline(&status.deadlines.sponsorship,)
            ))
            .text_size(px(11.0)),
        );
    } else if let Some(remaining) = countdown(&status.deadlines.sponsorship, chain_time) {
        footer_row =
            footer_row.child(app_muted_text(format!("Ends in {remaining}")).text_size(px(11.0)));
    }
    proposal_card("Sponsorship")
        .child(
            app_strong_text(format!(
                "{} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(15.0)),
        )
        .child(
            div()
                .w_full()
                .h(px(6.0))
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(theme::SURFACE_HOVER))
                .child(
                    div()
                        .w(px((300.0 - 28.0) * fraction))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(rgb(color)),
                ),
        )
        .child(footer_row)
}
pub(super) fn render_votes_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
    let for_fraction = split.map_or(0.0, ratio_fraction);
    let gap = if split.is_some_and(|value| value > 0 && value < 1_000) {
        2.0
    } else {
        0.0
    };
    let width = 272.0;
    let meter = div()
        .w_full()
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .children(split.map(|_| {
            div()
                .w(px((width - gap) * for_fraction))
                .h(px(6.0))
                .bg(rgb(theme::PRIMARY))
        }))
        .children(
            split
                .filter(|_| gap > 0.0)
                .map(|_| div().w(px(gap)).h(px(6.0)).bg(rgb(theme::SURFACE))),
        )
        .children(split.map(|_| div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER))));
    let legend = |label: &'static str, amount: U256, color: u32, fraction: Option<u16>| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(8.0)).rounded_full().bg(rgb(color)))
            .child(app_muted_text(label).flex_1())
            .child(
                app_text(format_compact_rail_amount_with_unit(amount))
                    .text_color(rgb(theme::TEXT))
                    .text_size(px(12.0)),
            )
            .children(
                fraction
                    .map(|value| app_muted_text(format_ratio_percent(value)).text_size(px(11.0))),
            )
    };
    proposal_card("Votes")
        .child(meter)
        .children([
            legend("For", proposal.proposal.yay_votes, theme::PRIMARY, split),
            legend(
                "Against",
                proposal.proposal.nay_votes,
                theme::DANGER,
                split.map(|value| 1_000 - value),
            ),
        ])
        .children(
            split
                .is_none()
                .then(|| app_muted_text("No votes").text_size(px(11.0))),
        )
        .child(
            app_muted_text(format!(
                "Sponsored {} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(format!(
                "Quorum: {} / {} · {}",
                format_compact_rail_amount(status.quorum_progress),
                format_compact_rail_amount(status.quorum),
                if status.quorum_met { "met" } else { "not met" },
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(match status.majority {
                wallet_ops::GovernanceMajorityResult::Yay => "Majority: yay",
                wallet_ops::GovernanceMajorityResult::Nay => "Majority: nay",
                wallet_ops::GovernanceMajorityResult::Tie => "Majority: tie",
            })
            .text_size(px(11.0)),
        )
        .children((status.stage == GovernanceProposalStage::Failed).then(|| {
            app_muted_text(if status.quorum_met {
                "Failed: yay majority not met"
            } else {
                "Failed: quorum not met"
            })
            .text_size(px(11.0))
        }))
}
fn timeline_step(
    icon: Option<IconName>,
    label: &'static str,
    value: String,
    color: u32,
    has_connector: bool,
) -> gpui::Div {
    let marker = icon.map_or_else(
        || {
            div()
                .size(px(10.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::BORDER))
        },
        |icon| {
            div()
                .size(px(14.0))
                .child(Icon::new(icon).text_color(rgb(color)))
        },
    );
    let mut body = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_strong_text(label).text_size(px(12.0)))
        .child(app_muted_text(value).text_size(px(11.0)));
    if has_connector {
        body = body.mb(px(12.0));
    }
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(14.0))
                .flex()
                .flex_col()
                .items_center()
                .child(marker)
                .children(has_connector.then(|| {
                    div()
                        .w(px(1.0))
                        .flex_1()
                        .min_h(px(12.0))
                        .bg(rgb(theme::BORDER_SUBTLE))
                })),
        )
        .child(body)
}

pub(super) fn timeline_deadline_completed(
    milestone: TimelineDeadline,
    chain_time: U256,
    deadline: U256,
) -> bool {
    match milestone {
        TimelineDeadline::VotingOpen | TimelineDeadline::ExecutionOpen => chain_time > deadline,
        TimelineDeadline::SponsorshipClose
        | TimelineDeadline::YayVotingEnd
        | TimelineDeadline::NayVotingEnd
        | TimelineDeadline::ExecutionClose => chain_time >= deadline,
    }
}

pub(super) fn timeline_sponsorship_completed(
    called: bool,
    chain_time: U256,
    deadline: U256,
) -> bool {
    called || timeline_deadline_completed(TimelineDeadline::SponsorshipClose, chain_time, deadline)
}

pub(super) fn render_timeline_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let called = !proposal.proposal.vote_call_time.is_zero();
    let sponsorship = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        (
            Some(IconName::CircleX),
            format!("Expired {}", format_deadline(&status.deadlines.sponsorship)),
            theme::TEXT_MUTED,
        )
    } else if timeline_sponsorship_completed(called, chain_time, status.deadlines.sponsorship) {
        (
            Some(IconName::CircleCheck),
            format_deadline(&status.deadlines.sponsorship),
            theme::SUCCESS,
        )
    } else {
        (
            None,
            format_deadline(&status.deadlines.sponsorship),
            theme::TEXT_MUTED,
        )
    };
    let mut steps = div().flex().flex_col();
    steps = steps.child(timeline_step(
        Some(IconName::CircleCheck),
        "Published",
        format_datetime_short(&proposal.proposal.publish_time),
        theme::SUCCESS,
        true,
    ));
    steps = steps.child(timeline_step(
        sponsorship.0,
        "Sponsorship closes",
        sponsorship.1,
        sponsorship.2,
        true,
    ));
    steps = steps.child(timeline_step(
        called.then_some(IconName::CircleCheck),
        "Voting called",
        if called {
            format_datetime_short(&proposal.proposal.vote_call_time)
        } else {
            "Not called".to_string()
        },
        if called {
            theme::SUCCESS
        } else {
            theme::TEXT_MUTED
        },
        true,
    ));
    if called {
        for (milestone, label, deadline) in [
            (
                TimelineDeadline::VotingOpen,
                "Voting opens",
                status.deadlines.voting_start,
            ),
            (
                TimelineDeadline::YayVotingEnd,
                "Yay voting ends",
                status.deadlines.yay_end,
            ),
            (
                TimelineDeadline::NayVotingEnd,
                "Nay voting ends",
                status.deadlines.nay_end,
            ),
            (
                TimelineDeadline::ExecutionOpen,
                "Execution opens",
                status.deadlines.execution_start,
            ),
            (
                TimelineDeadline::ExecutionClose,
                "Execution closes",
                status.deadlines.execution_end,
            ),
        ] {
            if let Some(deadline) = deadline {
                let completed = timeline_deadline_completed(milestone, chain_time, deadline);
                steps = steps.child(timeline_step(
                    completed.then_some(IconName::CircleCheck),
                    label,
                    format_deadline(&deadline),
                    if completed {
                        theme::SUCCESS
                    } else {
                        theme::TEXT_MUTED
                    },
                    true,
                ));
            }
        }
    }
    if proposal.proposal.executed {
        steps = steps.child(timeline_step(
            Some(IconName::CircleCheck),
            "Executed",
            "Confirmed on chain".to_string(),
            theme::SUCCESS,
            false,
        ));
    }
    proposal_card("Timeline").child(steps)
}
pub(super) fn render_details_card(proposal: &ResolvedProposal) -> gpui::Div {
    let proposer = proposal.proposal.proposer.to_checksum(None);
    let contract = proposal.proposal.contract_address.to_checksum(None);
    proposal_card("Details")
        .child(proposal_copy_control(
            proposal, "Proposer", proposer, "proposer",
        ))
        .child(proposal_copy_control(
            proposal,
            "IPFS CID",
            proposal.proposal.proposal_document.clone(),
            "document",
        ))
        .child(proposal_copy_control(
            proposal, "Contract", contract, "contract",
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_muted_text("Version").text_size(px(11.0)))
                .child(
                    app_text(match proposal.proposal.contract_version {
                        wallet_ops::GovernanceContractVersion::V2 => "V2 Voting",
                        wallet_ops::GovernanceContractVersion::V1 => "V1 VotingLegacy",
                    })
                    .text_size(px(12.0)),
                ),
        )
}
