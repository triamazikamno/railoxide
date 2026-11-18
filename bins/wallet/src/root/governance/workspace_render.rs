use super::*;

impl WalletRoot {
    pub(in crate::root) fn render_governance_workspace(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let compact_header = window.viewport_size().width < GOVERNANCE_COMPACT_HEADER_BREAKPOINT;
        let participant_trigger_width = if compact_header {
            GOVERNANCE_COMPACT_PARTICIPANT_TRIGGER_WIDTH
        } else {
            GOVERNANCE_PARTICIPANT_TRIGGER_WIDTH
        };
        let refresh_root = root.clone();
        let refreshing = match self.governance.tab {
            GovernanceTab::Proposals => self.proposals.refreshing,
            GovernanceTab::Staking => matches!(
                self.governance.staking.status,
                StakingRefreshStatus::Loading
            ),
        };
        let selected_index = usize::from(self.governance.tab == GovernanceTab::Staking);
        let tab_root = root.clone();
        self.sync_governance_participant_picker(window, cx);
        let selected_ids = self
            .governance_participants()
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect::<BTreeSet<_>>();
        let summary = participant_summary(
            &self.public_accounts,
            &selected_ids,
            self.selected_wallet_id.as_deref(),
        );
        let picker = Combobox::new(&self.governance.participant_picker)
            .small()
            .w(participant_trigger_width)
            .max_w_full()
            .p_0()
            .appearance(false)
            .menu_width(GOVERNANCE_PARTICIPANT_POPUP_WIDTH)
            .menu_max_h(px(350.0))
            .search_placeholder("search participating accounts")
            .empty(|_, _| app_muted_text("No visible accounts match this search."))
            .render_trigger(move |_, _, _| {
                let label = match summary.selected {
                    0 => "Enroll accounts...".to_owned(),
                    1 => "1 account enrolled".to_owned(),
                    count => format!("{count} accounts enrolled"),
                };
                div()
                    .w_full()
                    .h(px(24.0))
                    .min_w(px(0.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Icon::empty()
                            .path(USERS_ICON_PATH)
                            .size_4()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_MUTED)),
                    )
                    .child(
                        app_text(label)
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .when(summary.selected == 0, |this| {
                                this.text_color(rgb(theme::TEXT_MUTED))
                            }),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_PLACEHOLDER)),
                    )
            });
        let governance_tab = |label: &'static str, icon: Icon| {
            Tab::new().min_w(px(92.0)).child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon.size_4())
                    .child(label),
            )
        };
        let tabs = TabBar::new("governance-tabs")
            .underline()
            .w_full()
            .flex_none()
            .px(px(14.0))
            .selected_index(selected_index)
            .on_click(move |index, _window, cx| {
                let tab = if *index == 1 {
                    GovernanceTab::Staking
                } else {
                    GovernanceTab::Proposals
                };
                tab_root.update(cx, |root, cx| root.select_governance_tab(tab, cx));
            })
            .children([
                governance_tab("Proposals", Icon::new(IconName::File)),
                governance_tab(
                    "Staking",
                    Icon::empty().path(PIGGY_BANK_ICON_PATH).mr(px(2.0)),
                ),
            ]);
        let body = match self.governance.tab {
            GovernanceTab::Proposals => self
                .render_proposals_view(root, window, cx)
                .into_any_element(),
            GovernanceTab::Staking => self
                .render_staking_body(root, window, cx)
                .into_any_element(),
        };
        let body_container = div().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(body);
        let body_container = if self.governance.tab == GovernanceTab::Staking {
            body_container.overflow_y_scrollbar().into_any_element()
        } else {
            body_container.into_any_element()
        };
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(
                div()
                    .h(GOVERNANCE_HEADER_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(if compact_header { px(8.0) } else { px(12.0) })
                    .px(px(14.0))
                    .bg(rgb(theme::SURFACE))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        Icon::new(RailgunSidebarIcon::Landmark)
                            .size_5()
                            .text_color(rgb(theme::PRIMARY)),
                    )
                    .when(!compact_header, |this| {
                        this.child(app_strong_text("Governance").text_size(px(20.0)))
                    })
                    .child(self.render_wallet_selector())
                    .child(
                        div()
                            .w(participant_trigger_width)
                            .h(px(24.0))
                            .flex_none()
                            .child(picker),
                    )
                    .when(summary.inactive > 0, |this| {
                        this.child(app_status_tag(
                            format!("{} inactive", summary.inactive),
                            theme::WARNING,
                        ))
                    })
                    .child(self.render_chain_selector())
                    .child(div().flex_1().min_w(px(0.0)))
                    .child(app_refresh_button(
                        "governance-refresh",
                        "Refresh active governance tab",
                        refreshing,
                        true,
                        move |_window, cx| {
                            refresh_root.update(cx, |root, cx| match root.governance.tab {
                                GovernanceTab::Proposals => root.start_proposals_refresh(true, cx),
                                GovernanceTab::Staking => root.start_staking_refresh(cx),
                            });
                        },
                    )),
            )
            .child(tabs)
            .child(body_container)
    }

    fn render_staking_body(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Div {
        let mut content = div()
            .w(GOVERNANCE_CONTENT_WIDTH)
            .max_w_full()
            .mx_auto()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap_3();
        let viewport_width = window.viewport_size().width;
        let sidebar_is_narrow = viewport_width < crate::root::SIDEBAR_AUTO_COLLAPSE_WIDTH;
        let sidebar_collapsed = if sidebar_is_narrow {
            !self.sidebar_narrow_expanded
        } else {
            self.sidebar_manually_collapsed
        };
        let sidebar_width = if sidebar_collapsed {
            STAKING_COLLAPSED_SIDEBAR_WIDTH
        } else {
            crate::root::SIDEBAR_WIDTH
        };
        let workspace_width = (viewport_width - sidebar_width).max(px(0.0));
        let table_available_width = (workspace_width.min(GOVERNANCE_CONTENT_WIDTH)
            - STAKING_CARD_HORIZONTAL_CHROME)
            .max(px(0.0));
        let table_layout = staking_table_layout(table_available_width);
        self.governance.clear_stale_position_details();
        let account_header_compact =
            staking_account_header_compact(table_available_width, table_layout.compact);
        let state = &self.governance.staking;
        let table_horizontal_scrollbar = table_available_width < STAKING_TABLE_MIN_WIDTH;
        if self
            .selected_evm_chain()
            .and_then(governance_contracts)
            .is_none()
        {
            return content
                .child(app_status_tag(
                    "Staking is not deployed on this chain",
                    theme::TEXT_MUTED,
                ))
                .child(app_muted_text(
                    "Switch to Ethereum, BSC, or Polygon to view staking and rewards.",
                ));
        }
        match &state.status {
            StakingRefreshStatus::Error(error) => {
                return content
                    .child(Alert::error("governance-staking-error", error.to_string()).small())
                    .child(
                        app_button("governance-staking-retry", "Retry")
                            .small()
                            .on_click({
                                let root = root.clone();
                                move |_event, _window, cx| {
                                    root.update(cx, Self::start_staking_refresh);
                                }
                            }),
                    );
            }
            StakingRefreshStatus::Idle | StakingRefreshStatus::Loading
                if state.metrics.is_none() =>
            {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(app_muted_shimmer_text("Loading staking data..."));
            }
            _ => {}
        }
        if let Some(metrics) = state.metrics.as_ref() {
            let token = governance_contracts(self.last_evm_chain)
                .map_or(Address::ZERO, |contracts| contracts.governance_token);
            let reward_interval = state.reward_interval_countdown;
            let remaining = state.chain_time().and_then(|now| {
                reward_interval
                    .and_then(|countdown| remaining_reward_interval_seconds(countdown, now))
            });
            let total_staked = format_staking_amount(
                self.last_evm_chain,
                token,
                metrics.total_staked,
                &self.effective_token_registry,
            );
            let voting_power = format_staking_amount(
                self.last_evm_chain,
                token,
                metrics.total_voting_power,
                &self.effective_token_registry,
            );
            let next_reward_interval = remaining.map_or_else(
                || "Unavailable".to_owned(),
                |seconds| format_compact_duration(Duration::from_secs(seconds)),
            );
            let interval_tooltip = reward_interval.map_or_else(
                || "Reward interval unavailable".to_owned(),
                |countdown| {
                    format!(
                        "Reward interval {} ends in {}",
                        countdown.interval, next_reward_interval
                    )
                },
            );
            let next_reward_interval_text = div()
                .id("governance-next-reward-interval-tooltip")
                .tooltip(move |window, cx| Tooltip::new(interval_tooltip.clone()).build(window, cx))
                .child(app_muted_text("next reward interval"));
            content = content.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(app_strong_text(total_staked))
                            .child(app_muted_text("staked")),
                    )
                    .child(app_muted_text("·"))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(app_strong_text(voting_power))
                            .child(app_muted_text("active voting power")),
                    )
                    .child(app_muted_text("·"))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(next_reward_interval_text)
                            .child(app_muted_text("in"))
                            .child(app_strong_text(next_reward_interval)),
                    ),
            );
            if matches!(state.status, StakingRefreshStatus::Stale) {
                content = content.child(app_status_tag(
                    "Showing the last complete refresh",
                    theme::WARNING,
                ));
            }
        }
        let participants = self.governance_participants();
        content = content.child(app_strong_text("Positions & rewards").text_size(px(16.0)));
        if participants.is_empty() {
            content = content.child(render_staking_enrollment_prompt(root));
        }
        for account in participants {
            let uuid = account.public_account_uuid.clone();
            let inactive = account.status == PublicAccountStatus::Inactive;
            let actor_uuid = uuid.clone();
            let account_action_ready = state.account_action_ready(&uuid);
            let token = governance_contracts(self.last_evm_chain)
                .map_or(Address::ZERO, |contracts| contracts.governance_token);
            let account_view = state.accounts.get(&uuid);
            let mut rows = Vec::new();
            let projection = account_view
                .and_then(|view| view.stakes.as_ref().ok())
                .map_or_else(StakingAccountProjection::default, |stakes| {
                    staking_account_projection(stakes)
                });
            let stakes_available = account_view.is_some_and(|view| view.stakes.is_ok());
            if let Some(view) = account_view
                && let Ok(stakes) = &view.stakes
            {
                for stake in stakes {
                    rows.push(stake_table_row(
                        stake,
                        account.address,
                        inactive,
                        account_action_ready,
                        self.last_evm_chain,
                        token,
                        &self.effective_token_registry,
                        state.chain_time(),
                    ));
                }
            }
            let voting_power_display = match account_view {
                None => "Loading...".to_owned(),
                Some(view) => match &view.voting_power {
                    Ok(amount) => format_staking_amount(
                        self.last_evm_chain,
                        token,
                        *amount,
                        &self.effective_token_registry,
                    ),
                    Err(_) => "Unavailable".to_owned(),
                },
            };
            let qualifier = account_view.and_then(|view| {
                let (Ok(voting_power), Ok(_)) = (&view.voting_power, &view.stakes) else {
                    return None;
                };
                let received = projection.received_voting_power(*voting_power);
                let own_display = format_staking_amount(
                    self.last_evm_chain,
                    token,
                    projection.active_self_delegated,
                    &self.effective_token_registry,
                );
                let received_display = format_staking_amount(
                    self.last_evm_chain,
                    token,
                    received,
                    &self.effective_token_registry,
                );
                let delegated_display = format_staking_amount(
                    self.last_evm_chain,
                    token,
                    projection.active_external_delegated,
                    &self.effective_token_registry,
                );
                match (
                    received.is_zero(),
                    projection.active_external_delegated.is_zero(),
                ) {
                    (true, true) => None,
                    (false, true) => Some(format!(
                        "includes {received_display} received from other accounts"
                    )),
                    (true, false) => Some(format!(
                        "{} staked · delegated away",
                        format_staking_amount(
                            self.last_evm_chain,
                            token,
                            projection.unclaimed_principal,
                            &self.effective_token_registry,
                        )
                    )),
                    (false, false) => Some(format!(
                        "{own_display} own · {received_display} received · {delegated_display} delegated away"
                    )),
                }
            });
            let identity = div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_strong_text(
                        public_account_display_label(&account)
                            .unwrap_or_else(|| "Public account".to_owned()),
                    )
                    .min_w(px(0.0))
                    .truncate(),
                )
                .child(app_muted_text(short_address(&account.address)).flex_none())
                .child(clipboard_with_toast(
                    staking_control_id(&actor_uuid, "account-copy", "address"),
                    format!("{:#x}", account.address),
                ))
                .when(inactive, |this| {
                    this.child(app_status_tag("Inactive · read-only", theme::TEXT_MUTED))
                });
            let header = render_staking_account_header(
                identity,
                voting_power_metric(&voting_power_display, account_view.is_none(), qualifier),
                account_header_compact,
                None,
            )
            .p_3();
            let mut card = div()
                .flex()
                .flex_col()
                .rounded_md()
                .bg(rgb(theme::SURFACE))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(header);
            let wallet_balance = state
                .current_account_ids
                .contains(&uuid)
                .then_some(account_view)
                .flatten()
                .and_then(|view| view.balance.as_ref().ok())
                .copied()
                .filter(|balance| !balance.is_zero());
            if let Some(_view) = account_view {
                if stakes_available && !rows.is_empty() {
                    let row_count = rows.len();
                    let table_height = STAKING_OVERVIEW_ROW_HEIGHT * row_count.clamp(1, 5);
                    let table_scrollbar = row_count > 5;
                    let table = {
                        let table_key =
                            format!("governance-stakes-{}-{}", self.last_evm_chain, uuid);
                        if let Some(table) = self.governance.staking_tables.get(&table_key) {
                            let table = table.clone();
                            table.update(cx, |table_state, cx| {
                                let (rows_changed, widths_changed) = {
                                    let delegate = table_state.delegate_mut();
                                    let rows_changed = delegate.set_rows(rows);
                                    let widths_changed = delegate.set_layout(&table_layout);
                                    (rows_changed, widths_changed)
                                };
                                if widths_changed {
                                    table_state.refresh(cx);
                                } else if rows_changed {
                                    cx.notify();
                                }
                            });
                            table
                        } else {
                            let delegate_root = root.downgrade();
                            let delegate_uuid = actor_uuid.clone();
                            let table = cx.new(|cx| {
                                TableState::new(
                                    StakeTableDelegate::new(
                                        delegate_root,
                                        delegate_uuid,
                                        account.address,
                                        rows,
                                        table_layout.clone(),
                                    ),
                                    window,
                                    cx,
                                )
                                .sortable(false)
                                .row_selectable(true)
                                .col_selectable(false)
                                .col_movable(false)
                                .col_resizable(false)
                            });
                            self.governance
                                .staking_tables
                                .insert(table_key, table.clone());
                            table
                        }
                    };
                    let vertical_scroll_handle = table.read(cx).vertical_scroll_handle.clone();
                    let table_scroller = div()
                        .w_full()
                        .h(table_height)
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .relative()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .id(staking_control_id(&actor_uuid, "table-scroll", "positions"));
                    let table_scroller = table_scroller.child(
                        DataTable::new(&table)
                            .large()
                            .bordered(false)
                            .scrollbar_visible(false, table_horizontal_scrollbar),
                    );
                    let table_scroller = if table_scrollbar {
                        table_scroller.child(
                            div()
                                .occlude()
                                .absolute()
                                .top_0()
                                .right_0()
                                .bottom_0()
                                .w(STAKING_TABLE_SCROLLBAR_WIDTH)
                                .child(Scrollbar::vertical(&vertical_scroll_handle)),
                        )
                    } else {
                        table_scroller
                    };
                    card = card.child(table_scroller);
                } else if !stakes_available {
                    card = card.child(
                        div()
                            .px_3()
                            .py_2()
                            .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                            .flex()
                            .items_center()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .child(app_muted_text("Stake rows unavailable for this account.")),
                    );
                } else if wallet_balance.is_none() {
                    card = card.child(
                        div()
                            .px_3()
                            .py_2()
                            .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                            .flex()
                            .items_center()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .child(app_muted_text("No staking positions yet.")),
                    );
                }
            } else {
                card = card.child(
                    div()
                        .px_3()
                        .py_2()
                        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                        .flex()
                        .items_center()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .child(app_muted_shimmer_text("Loading account stakes...")),
                );
            }
            if !inactive && let Some(balance) = wallet_balance {
                let stake_root = root.clone();
                let stake_uuid = actor_uuid.clone();
                let stake_actor = account.address;
                let stake_button =
                    app_button_base(staking_control_id(&actor_uuid, "stake", "account"))
                        .primary()
                        .small()
                        .disabled(!state.global_action_ready());
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .child(app_muted_text(format!(
                            "{} in wallet",
                            format_token_amount_for_display(
                                self.last_evm_chain,
                                token,
                                balance,
                                Some(&self.effective_token_registry),
                            )
                        )))
                        .child(
                            div().flex_1().min_w(px(0.0)).flex().justify_end().child(
                                stake_button
                                    .child("Stake")
                                    .on_click(move |_event, window, cx| {
                                        stake_root.update(cx, |root, cx| {
                                            root.open_staking_action(
                                                stake_uuid.as_str(),
                                                stake_actor,
                                                StakingActionKind::Stake,
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                        ),
                );
            }
            let reward_tokens = governance_contracts(self.last_evm_chain)
                .map_or(&[][..], |contracts| contracts.reward_tokens);
            let mut reward_token_addresses = reward_tokens
                .iter()
                .map(|token| token.token)
                .collect::<Vec<_>>();
            reward_token_addresses.sort();
            let all_rewards_resolved_zero = !reward_tokens.is_empty()
                && reward_tokens.iter().all(|token| {
                    let key = (uuid.clone(), token.token);
                    state.current_reward_keys.contains(&key)
                        && matches!(state.rewards.get(&key), Some(RewardView::Zero))
                });
            if all_rewards_resolved_zero {
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .px_3()
                        .py_2()
                        .child(app_strong_text("Rewards").text_size(px(12.0)))
                        .child(app_muted_text("Nothing to claim yet.").text_size(px(12.0))),
                );
            } else {
                let positive_reward_tokens = reward_token_addresses
                    .iter()
                    .copied()
                    .filter(|&token| state.positive_reward_evidence(&uuid, token).is_some())
                    .collect::<Vec<_>>();
                let positive_token_count = positive_reward_tokens.len();
                let (total_unclaimed, total_loading) = match reward_usd_total(
                    self.last_evm_chain,
                    &uuid,
                    &reward_token_addresses,
                    &state.rewards,
                    &state.current_reward_keys,
                    &self.public_broadcaster_anchor_cache,
                ) {
                    RewardUsdState::Loading => ("Loading...".to_owned(), true),
                    RewardUsdState::Unavailable => ("USD unavailable".to_owned(), false),
                    RewardUsdState::Value(value) => (
                        format!("{} unclaimed", format_usd_micro_value(value)),
                        false,
                    ),
                };
                let claim_selection = StakingActionSelection {
                    actor_uuid: uuid.clone(),
                    actor: account.address,
                    kind: StakingActionKind::RewardClaimSelected {
                        tokens: positive_reward_tokens.clone(),
                    },
                };
                let claim_ready = positive_token_count >= 1
                    && state.action_selection_ready(&claim_selection)
                    && !inactive
                    && self.governance.action_flow.selection.is_none();
                let claim_root = root.clone();
                let claim_uuid = uuid.clone();
                let claim_actor = account.address;
                let claim_tokens = positive_reward_tokens;
                let claim_button =
                    app_button_base(staking_control_id(&uuid, "reward-claim", "account"))
                        .primary()
                        .small()
                        .child("Claim…")
                        .on_click(move |_event, window, cx| {
                            let tokens = claim_tokens.clone();
                            claim_root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    claim_uuid.as_str(),
                                    claim_actor,
                                    StakingActionKind::RewardClaimSelected { tokens },
                                    window,
                                    cx,
                                );
                            });
                        });
                let reward_rows = reward_tokens.iter().filter_map(|token| {
                    let resolved_reward = state.resolved_reward(&uuid, token.token);
                    if matches!(resolved_reward, Some(RewardView::Zero)) {
                        return None;
                    }
                    Some(render_reward_row(
                        self.last_evm_chain,
                        token.symbol,
                        token.token,
                        resolved_reward,
                        &self.effective_token_registry,
                        &self.public_broadcaster_anchor_cache,
                    ))
                });
                let reward_header = div()
                    .flex()
                    .items_center()
                    .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.0))
                            .child(app_strong_text("Rewards").text_size(px(12.0)))
                            .child(
                                if total_loading {
                                    app_muted_shimmer_text(total_unclaimed)
                                } else {
                                    app_muted_text(total_unclaimed)
                                }
                                .text_size(px(11.0)),
                            ),
                    )
                    .when(claim_ready, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .justify_end()
                                .child(claim_button),
                        )
                    });
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .px_3()
                        .child(reward_header)
                        .children(reward_rows),
                );
            }
            content = content.child(card);
        }
        content
    }
}
