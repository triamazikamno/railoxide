use super::*;

impl WalletRoot {
    pub(in crate::root) fn render_proposal_action_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: gpui::Pixels,
        cx: &App,
    ) -> gpui::Div {
        let proposal = self.proposals.selected.as_ref().and_then(|selection| {
            self.proposals
                .rows(selection.page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
        });
        let Some(proposal) = proposal else {
            let close_root = root;
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(Alert::error(
                    "governance-action-stale-selection",
                    "The selected proposal is no longer available. Refresh before reviewing this action.",
                ))
                .child(
                    div().flex().justify_end().child(
                        app_button_base("governance-action-stale-cancel")
                            .ghost()
                            .small()
                            .child("Cancel")
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, Self::close_proposal_action);
                                window.close_dialog(cx);
                            }),
                    ),
                );
        };
        let token = railgun_ui::governance_contracts(self.last_evm_chain)
            .map_or(Address::ZERO, |contracts| contracts.governance_token);
        let decimals = self
            .effective_token_registry
            .get(self.last_evm_chain, &token)
            .map(|info| info.decimals);
        render_proposal_action_form(
            &root,
            self,
            proposal,
            decimals,
            content_width,
            self.proposals.chain_time(),
            cx,
        )
            .unwrap_or_else(|| {
                let close_root = root;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Alert::error(
                        "governance-action-stale-context",
                        "This proposal action is no longer valid for the current account or chain context. Refresh before trying again.",
                    ))
                    .child(
                        div().flex().justify_end().child(
                            app_button_base("governance-action-stale-context-cancel")
                                .ghost()
                                .small()
                                .child("Cancel")
                                .on_click(move |_event, window, cx| {
                                    close_root.update(cx, Self::close_proposal_action);
                                    window.close_dialog(cx);
                                }),
                        ),
                    )
            })
    }

    pub(in crate::root) fn render_proposals_view(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_detail = self.proposals.selected.as_ref().and_then(|selection| {
            self.proposals
                .rows(selection.page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
        });
        let has_active_detail = active_detail.is_some();
        let active_content = if let Some(proposal) = active_detail {
            self.render_proposal_detail(root, proposal, window, cx)
                .into_any_element()
        } else {
            self.render_proposal_list(root, window, cx)
                .into_any_element()
        };
        let keyboard_root = root.clone();
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" && has_active_detail {
                    keyboard_root.update(cx, |root, cx| {
                        root.clear_selected_proposal(cx);
                    });
                    cx.stop_propagation();
                    return;
                }
                if has_active_detail || event.keystroke.modifiers.modified() {
                    return;
                }
                let is_left = match event.keystroke.key.as_str() {
                    "left" => true,
                    "right" => false,
                    _ => return,
                };
                keyboard_root.update(cx, |root, cx| {
                    let page = if is_left {
                        root.proposals.current_page.checked_sub(1)
                    } else {
                        next_proposal_page(root.proposals.current_page, root.proposals.total_pages)
                    };
                    if let Some(page) = page {
                        root.select_proposals_page(page, cx);
                    }
                });
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(active_content),
            )
            .into_any_element()
    }
    fn render_proposal_list(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let page = self.proposals.current_page;
        let row_count = self.proposals.rows(page).map_or(0, <[_]>::len);
        let list = if row_count > 0 {
            let list = self.ensure_proposal_list(page, root, window, cx);
            let focus_list_on_render = self.proposals.focus_list_on_render;
            list.update(cx, |list, cx| {
                if let Some(index) = list.selected_index()
                    && index.row >= row_count
                {
                    list.set_selected_index(
                        Some(gpui_component::IndexPath::default().row(row_count - 1)),
                        window,
                        cx,
                    );
                }
                if focus_list_on_render {
                    if list.selected_index().is_none() {
                        list.set_selected_index(
                            Some(gpui_component::IndexPath::default()),
                            window,
                            cx,
                        );
                    }
                    list.focus(window, cx);
                }
            });
            if focus_list_on_render {
                self.proposals.focus_list_on_render = false;
            }
            Some(list)
        } else {
            None
        };
        let state = &self.proposals;
        let mut content = div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .items_center()
            .bg(rgb(theme::SURFACE_ELEVATED));
        let mut body = div()
            .w(CONTENT_WIDTH)
            .max_w_full()
            .mx_auto()
            .flex_1()
            .min_h(px(0.0))
            .p(px(16.0))
            .flex()
            .flex_col();
        let loading_without_rows =
            (state.loading || state.loading_pages.contains(&page)) && state.rows(page).is_none();
        let mut list_content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_3()
            .child(governance_permissionless_warning());
        let total = state
            .overview
            .as_ref()
            .and_then(|overview| total_proposals(overview).ok());
        let show_toolbar = total.is_some() || state.total_pages > 1;
        if show_toolbar {
            let mut toolbar = div().w_full().flex().items_center().justify_between();
            if let Some(total) = total {
                toolbar =
                    toolbar.child(app_muted_text(format!("{total} proposals")).text_size(px(12.0)));
            }
            if state.total_pages > 1 {
                let previous_root = root.clone();
                let next_root = root.clone();
                toolbar = toolbar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_button_base("wallet-proposals-previous")
                                .accessibility_label("Previous proposals page")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page == 0)
                                .icon(IconName::ChevronLeft)
                                .on_click(move |_event, _window, cx| {
                                    previous_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page.saturating_sub(1);
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        )
                        .child(
                            app_muted_text(format!(
                                "{} / {}",
                                state.current_page + 1,
                                state.total_pages
                            ))
                            .text_size(px(12.0)),
                        )
                        .child(
                            app_button_base("wallet-proposals-next")
                                .accessibility_label("Next proposals page")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page + 1 >= state.total_pages)
                                .icon(IconName::ChevronRight)
                                .on_click(move |_event, _window, cx| {
                                    next_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page + 1;
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        ),
                );
            }
            list_content = list_content.child(toolbar);
        } else if loading_without_rows {
            list_content = list_content.child(div().h(px(16.0)).flex_none());
        }
        if loading_without_rows {
            let mut skeleton_rows = div().flex().flex_col().gap_3();
            for _ in 0..PROPOSALS_PAGE_SIZE {
                skeleton_rows = skeleton_rows.child(proposal_skeleton_row());
            }
            list_content = list_content.child(skeleton_rows);
        } else if state.overview.is_none() && state.checked && state.error.is_none() {
            list_content = list_content.child(proposals_empty_state(
                "Governance is not deployed on this chain.",
            ));
        } else if state.rows(page).is_some_and(<[_]>::is_empty) && state.error.is_none() {
            list_content =
                list_content.child(proposals_empty_state("No governance proposals found."));
        } else if let Some(list) = list {
            list_content = list_content.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(List::new(&list).size_full()),
            );
        } else if state.error.is_some() {
            list_content =
                list_content.child(proposals_empty_state("Proposal data could not be loaded."));
        }
        if let Some(error) = state.error.as_ref() {
            let retry_root = root.clone();
            list_content = list_content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Alert::error("wallet-proposals-error", error.to_string()).small())
                    .child(
                        app_button("wallet-proposals-retry", "Retry")
                            .small()
                            .on_click(move |_event, _window, cx| {
                                retry_root.update(cx, Self::retry_proposals);
                            }),
                    ),
            );
        }
        body = body.child(list_content);
        let body = if row_count > 0 {
            body.into_any_element()
        } else {
            body.overflow_y_scrollbar().into_any_element()
        };
        content = content.child(body);
        content.into_any_element()
    }
    pub(super) fn render_proposal_row(
        proposal: &ResolvedProposal,
        chain_time: U256,
        selected: bool,
    ) -> ListItem {
        let status = proposal.status(chain_time);
        let title = proposal.document().map(|document| {
            if document.available {
                ProposalRowTitle::Available(if document.title.is_empty() {
                    format!("Proposal #{}", proposal.proposal.index)
                } else {
                    document.title.clone()
                })
            } else {
                ProposalRowTitle::Unavailable
            }
        });
        let identity = proposal.identity();
        let group_name = proposal_row_group(&identity);
        let row_id = SharedString::from(format!(
            "wallet-proposal-row-{}-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.contract_address,
            proposal.proposal.index,
        ));
        let row = div()
            .group(group_name)
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px(px(14.0))
            .py(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_muted_text(format!("#{}", proposal.proposal.index))
                            .text_color(rgb(theme::TEXT_SUBTLE))
                            .text_size(px(15.0))
                            .line_height(px(18.0)),
                    )
                    .child(proposal_row_title(title))
                    .child(div().ml_auto().child(app_status_tag(
                        proposal_stage_label(status.stage),
                        proposal_stage_color(status.stage),
                    ))),
            )
            .child(proposal_row_meta(proposal, chain_time));
        let row = if selected {
            row.bg(rgb(theme::SURFACE))
        } else {
            row
        };
        ListItem::new(row_id)
            .p_0()
            .mb_3()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(row)
    }
    fn render_proposal_detail(
        &self,
        root: &Entity<Self>,
        proposal: &ResolvedProposal,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let back_root = root.clone();
        let chain_time = self.proposals.chain_time();
        let status = proposal.status(chain_time);
        let detail_tab = self
            .proposals
            .selected
            .as_ref()
            .map_or_else(ProposalDetailTab::default, |selection| selection.tab);
        let proposer = proposal.proposal.proposer.to_checksum(None);
        let description_id = SharedString::from(format!(
            "wallet-proposal-description-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.index,
        ));
        let detail_scroll = self.proposals.detail_scroll_handle.clone();
        let mut detail_scroller = div()
            .id("wallet-proposals-detail-scroll")
            .role(gpui::accesskit::Role::Group)
            .aria_label("Proposal details")
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .track_scroll(&detail_scroll)
            .overflow_y_scroll()
            .vertical_scrollbar(&detail_scroll)
            .relative();
        detail_scroller.style().restrict_scroll_to_axis = Some(true);
        detail_scroller
            .bg(rgb(theme::SURFACE_ELEVATED))
            .track_focus(&self.proposal_detail_focus)
            .child(
                div()
                    .w(CONTENT_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(governance_permissionless_warning())
                    .child(
                        div().flex().items_center().flex_wrap().gap_2().child(
                            div()
                                .flex()
                                .items_center()
                                .flex_none()
                                .gap_2()
                                .child(
                                    app_button_base("wallet-proposals-back")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .icon(IconName::ArrowLeft)
                                        .accessibility_label("Back to governance")
                                        .tooltip("Back to governance")
                                        .on_click(move |_event, _window, cx| {
                                            back_root.update(cx, |root, cx| {
                                                root.clear_selected_proposal(cx);
                                            });
                                        }),
                                )
                                .child(
                                    app_muted_text(format!(
                                        "Proposal #{}",
                                        proposal.proposal.index
                                    ))
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0)),
                                )
                                .child(app_status_tag(
                                    proposal_stage_label(status.stage),
                                    proposal_stage_color(status.stage),
                                ))
                                .children(
                                    (proposal.proposal.contract_version
                                        == wallet_ops::GovernanceContractVersion::V1)
                                        .then(|| app_status_tag("V1", theme::TEXT_MUTED)),
                                ),
                        ),
                    )
                    .child(proposal_detail_title(proposal))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(app_muted_text("Proposed by").text_size(px(12.0)))
                                    .child(
                                        app_text(spend_authorization_recipient_display(&proposer))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0)),
                                    )
                                    .child(
                                        div()
                                            .id(proposal_copy_id(
                                                proposal,
                                                "proposer-meta",
                                                "action",
                                            ))
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy Proposer").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                proposal_copy_id(
                                                    proposal,
                                                    "proposer-meta",
                                                    "clipboard",
                                                ),
                                                proposer.clone(),
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .flex_none()
                                    .child(app_muted_text("·").text_size(px(12.0)))
                                    .child(
                                        app_muted_text(format!(
                                            "Published {}",
                                            format_datetime_short(&proposal.proposal.publish_time)
                                        ))
                                        .text_size(px(12.0)),
                                    ),
                            ),
                    )
                    .child(render_proposal_participation_card(
                        root,
                        self,
                        proposal,
                        status.stage,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .items_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(render_proposal_detail_tabs(
                                        root,
                                        detail_tab,
                                        proposal.proposal.actions.len(),
                                        &proposal.identity(),
                                    ))
                                    .child(match detail_tab {
                                        ProposalDetailTab::Description => proposal_document_card(
                                            proposal,
                                            description_id,
                                            &self.proposals.table_scroll_handles,
                                            window,
                                            cx,
                                        )
                                        .into_any_element(),
                                        ProposalDetailTab::Actions => render_proposal_actions_card(
                                            root,
                                            proposal,
                                            self.proposals.chain_id,
                                            &self.proposals.expanded_calldata,
                                            &self.public_broadcaster_anchor_cache,
                                            &self.effective_token_registry,
                                            self.effective_chain_configs
                                                .get(self.proposals.chain_id),
                                            &self.public_accounts,
                                            self.view_session
                                                .as_ref()
                                                .map(|_| self.public_address_book.as_slice()),
                                        )
                                        .into_any_element(),
                                    }),
                            )
                            .child(
                                div()
                                    .w(px(300.0))
                                    .flex_none()
                                    .pt(px(44.0))
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .children(match status.stage {
                                        GovernanceProposalStage::AwaitingSponsorship
                                        | GovernanceProposalStage::ReadyToCallVote
                                        | GovernanceProposalStage::SponsorshipExpired
                                        | GovernanceProposalStage::VoteCallExpired => {
                                            vec![render_sponsorship_card(proposal, chain_time)]
                                        }
                                        _ => vec![render_votes_card(proposal, chain_time)],
                                    })
                                    .child(render_timeline_card(proposal, chain_time))
                                    .child(render_details_card(proposal)),
                            ),
                    ),
            )
            .into_any_element()
    }
}
