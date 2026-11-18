use super::*;

pub(super) const fn proposal_stage_color(stage: GovernanceProposalStage) -> u32 {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::VotingDelay => theme::WARNING,
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => theme::BLUE,
        GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::Executed => theme::SUCCESS,
        GovernanceProposalStage::Failed => theme::DANGER,
        GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired
        | GovernanceProposalStage::ExecutionExpired => theme::TEXT_MUTED,
    }
}
pub(super) fn governance_permissionless_warning() -> Alert {
    Alert::warning(
        "wallet-governance-permissionless-warning",
        "Governance is permissionless. Proposals may contain misleading or malicious content. Exercise caution, verify claims independently, and do your own research before taking action.",
    )
    .small()
}
pub(super) fn proposals_empty_state(message: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .p(px(48.0))
        .child(
            Icon::new(IconName::Inbox)
                .size_6()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
        .child(app_muted_text(message))
}
pub(super) fn proposal_row_title(title: Option<ProposalRowTitle>) -> gpui::Div {
    match title {
        Some(ProposalRowTitle::Available(title)) if !title.is_empty() => app_strong_text(title)
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(15.0))
            .line_height(px(18.0))
            .truncate(),
        Some(ProposalRowTitle::Available(_) | ProposalRowTitle::Unavailable) => {
            app_muted_text("Document unavailable")
                .flex_1()
                .min_w(px(0.0))
                .italic()
        }
        None => div()
            .flex_1()
            .min_w(px(0.0))
            .child(Skeleton::new().h(px(14.0)).w(px(180.0))),
    }
}
pub(super) fn proposal_row_meta(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let mut left = div()
        .flex()
        .items_center()
        .gap_1()
        .flex_1()
        .min_w(px(0.0))
        .child(
            app_muted_text(format!(
                "Published {}",
                format_date_short(&proposal.proposal.publish_time)
            ))
            .text_size(px(12.0)),
        );
    if let Some(remaining) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship => {
            countdown(&status.deadlines.sponsorship, chain_time)
                .map(|remaining| format!(" · Sponsorship ends in {remaining}"))
        }
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => {
            list_voting_deadline(&status).and_then(|(deadline, label)| {
                countdown(&deadline, chain_time).map(|remaining| format!(" · {label} {remaining}"))
            })
        }
        GovernanceProposalStage::VotingDelay => countdown(
            &status
                .deadlines
                .voting_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Voting opens in {remaining}")),
        GovernanceProposalStage::PassedAwaitingExecution => countdown(
            &status
                .deadlines
                .execution_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution opens in {remaining}")),
        GovernanceProposalStage::PassedExecutable => countdown(
            &status
                .deadlines
                .execution_end
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution closes in {remaining}")),
        _ => None,
    } {
        left = left.child(app_muted_text(remaining).text_size(px(12.0)));
    }
    if proposal.proposal.contract_version == wallet_ops::GovernanceContractVersion::V1 {
        left = left.child(app_status_tag("V1", theme::TEXT_MUTED));
    }
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt(px(4.0))
        .child(left)
        .child(render_list_meter(proposal, chain_time))
}
fn render_list_meter(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let group_name = proposal_row_group(&proposal.identity());
    let (for_amount, against_amount, caption, for_per_mille, color) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired => {
            let per_mille = sponsorship_per_mille(proposal);
            let percent = (u32::from(per_mille) + 5) / 10;
            (
                None,
                None,
                format!(
                    "{} / {} RAIL sponsored · {percent}%",
                    format_compact_rail_amount(proposal.proposal.sponsorship),
                    format_compact_rail_amount(proposal.rules.sponsor_threshold)
                ),
                per_mille,
                if matches!(
                    status.stage,
                    GovernanceProposalStage::SponsorshipExpired
                        | GovernanceProposalStage::VoteCallExpired
                ) {
                    theme::TEXT_MUTED
                } else {
                    theme::PRIMARY
                },
            )
        }
        _ => {
            let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
            let caption = split.map_or_else(
                || "No votes".to_string(),
                |_| {
                    format!(
                        "{} for · {} against",
                        format_compact_rail_amount(proposal.proposal.yay_votes),
                        format_compact_rail_amount(proposal.proposal.nay_votes)
                    )
                },
            );
            (
                Some(proposal.proposal.yay_votes),
                Some(proposal.proposal.nay_votes),
                caption,
                split.unwrap_or(0),
                theme::PRIMARY,
            )
        }
    };
    let track = div()
        .w(px(230.0))
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .child(
            if let (Some(yay), Some(nay)) = (for_amount, against_amount)
                && let Some(split) = vote_split(yay, nay)
            {
                let total_width = 230.0;
                let gap = if split > 0 && split < 1_000 { 2.0 } else { 0.0 };
                let split = ratio_fraction(split);
                div()
                    .flex()
                    .w(px(total_width))
                    .h(px(6.0))
                    .child(
                        div()
                            .w(px((total_width - gap) * split))
                            .h(px(6.0))
                            .bg(rgb(theme::PRIMARY)),
                    )
                    .child(
                        div()
                            .w(px(gap))
                            .h(px(6.0))
                            .bg(rgb(theme::SURFACE))
                            .group_hover(group_name, |style| {
                                style.bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                            }),
                    )
                    .child(div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER)))
            } else {
                div()
                    .w(px((230.0 * ratio_fraction(for_per_mille)).clamp(0.0, 230.0)))
                    .h(px(6.0))
                    .bg(rgb(color))
            },
        );
    div()
        .w(px(230.0))
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(track)
        .child(app_muted_text(caption).text_size(px(11.0)))
}
pub(super) fn proposal_skeleton_row() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .h(px(72.0))
        .gap_3()
        .px(px(14.0))
        .py(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(Skeleton::new().h(px(14.0)).w(px(240.0)))
        .child(Skeleton::new().secondary().h(px(10.0)).w(px(160.0)))
}
pub(super) fn proposal_detail_title(proposal: &ResolvedProposal) -> gpui::Div {
    match proposal.document() {
        None => div().child(Skeleton::new().h(px(22.0)).w(px(360.0))),
        Some(document) if document.available && !document.title.is_empty() => {
            app_strong_text(document.title.clone())
                .text_size(px(22.0))
                .whitespace_normal()
        }
        Some(_) => {
            app_strong_text(format!("Proposal #{}", proposal.proposal.index)).text_size(px(22.0))
        }
    }
}

pub(super) fn render_proposal_blocks(
    blocks: &[ProposalBlock],
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    for (index, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}-{index}");
        let element = match block {
            ProposalBlock::Markdown(source) => TextView::markdown(
                SharedString::from(format!("{description_id}-text-{block_path}")),
                source.clone(),
            )
            .selectable(true)
            // A link that survives sanitizing must not open the browser outside Tor.
            .on_link_click(|_, _, _, _| {})
            .into_any_element(),
            ProposalBlock::Table(table) => {
                let scroll = table_scroll_handles
                    .get(&proposal_table_scroll_key(identity, table.ordinal))
                    .cloned()
                    .unwrap_or_else(ScrollHandle::new);
                let table_id = format!(
                    "{description_id}-table-{}-{}",
                    identity.contract_address, table.ordinal
                );
                let text = TextView::markdown(
                    SharedString::from(format!("{table_id}-text")),
                    table.render_source.clone(),
                )
                .w(px(f32::from(table.render_width_px)))
                .min_w_full()
                .flex_none()
                .selectable(true)
                .on_link_click(|_, _, _, _| {});
                let mut table_scroller = div()
                    .id(SharedString::from(format!("{table_id}-scroll")))
                    .w_full()
                    .min_w(px(0.0))
                    .track_scroll(&scroll)
                    .overflow_x_scroll()
                    .horizontal_scrollbar(&scroll)
                    .relative();
                table_scroller.style().restrict_scroll_to_axis = Some(true);
                div()
                    .id(SharedString::from(format!("{table_id}-wrapper")))
                    .w_full()
                    .min_w(px(0.0))
                    .child(table_scroller.child(text))
                    .into_any_element()
            }
            ProposalBlock::List(list) => render_proposal_list(
                list,
                description_id,
                &block_path,
                identity,
                table_scroll_handles,
            ),
            ProposalBlock::Blockquote(blockquote) => div()
                .id(SharedString::from(format!(
                    "{description_id}-blockquote-{block_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .pl(px(12.0))
                .border_l_2()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(render_proposal_blocks(
                    &blockquote.blocks,
                    description_id,
                    &block_path,
                    identity,
                    table_scroll_handles,
                ))
                .into_any_element(),
        };
        container = container.child(element);
    }
    container.into_any_element()
}

fn render_proposal_list(
    list: &ProposalList,
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    let start = list.start.unwrap_or(1);
    for (index, item) in list.items.iter().enumerate() {
        let marker = if let Some(checked) = item.checked {
            if checked { "[x]" } else { "[ ]" }.to_owned()
        } else if list.ordered {
            format!("{}.", start.saturating_add(index as u32))
        } else {
            "•".to_owned()
        };
        let item_path = format!("{path}-{index}");
        let body = render_proposal_blocks(
            &item.blocks,
            description_id,
            &item_path,
            identity,
            table_scroll_handles,
        );
        container = container.child(
            div()
                .id(SharedString::from(format!(
                    "{description_id}-list-row-{item_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .child(
                    div()
                        .w(px(32.0))
                        .flex_none()
                        .child(app_muted_text(marker).text_size(px(14.0))),
                )
                .child(div().flex_1().min_w(px(0.0)).child(body)),
        );
    }
    container.into_any_element()
}

pub(super) fn render_proposal_detail_tabs(
    root: &Entity<WalletRoot>,
    selected: ProposalDetailTab,
    action_count: usize,
    identity: &ProposalIdentity,
) -> impl IntoElement {
    let selected_index = match selected {
        ProposalDetailTab::Description => 0,
        ProposalDetailTab::Actions => 1,
    };
    let tab_root = root.clone();
    TabBar::new(SharedString::from(format!(
        "wallet-proposal-detail-tabs-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )))
    .underline()
    .w_full()
    .flex_none()
    .selected_index(selected_index)
    .on_click(move |index, _window, cx| {
        let tab = match *index {
            0 => ProposalDetailTab::Description,
            1 => ProposalDetailTab::Actions,
            _ => return,
        };
        tab_root.update(cx, |root, cx| root.select_proposal_detail_tab(tab, cx));
    })
    .children([
        Tab::new().min_w(px(112.0)).label("Description"),
        Tab::new()
            .min_w(px(112.0))
            .label(format!("Actions ({action_count})")),
    ])
}

pub(super) fn proposal_address_badge(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .px(px(5.0))
        .py(px(2.0))
        .text_size(px(10.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .whitespace_normal()
        .child(label.into())
}
