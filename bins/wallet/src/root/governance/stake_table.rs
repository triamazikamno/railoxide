use super::*;

pub(super) fn render_staking_enrollment_prompt(root: &Entity<WalletRoot>) -> gpui::Div {
    let picker_root = root.clone();
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Enroll accounts to get started"))
        .child(app_muted_text(
            "Choose Public accounts to view their positions and rewards.",
        ))
        .child(
            app_button_base("governance-staking-enroll")
                .outline()
                .small()
                .child("Enroll accounts")
                .on_click(move |_event, window, cx| {
                    picker_root.update(cx, |root, cx| {
                        root.governance
                            .participant_picker
                            .read(cx)
                            .focus_handle(cx)
                            .focus(window, cx);
                        window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                    });
                }),
        )
}

pub(super) fn voting_power_metric(
    value: &str,
    loading: bool,
    qualifier: Option<String>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w(px(0.0))
        .items_end()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_1()
                .child(if loading {
                    app_shimmer_text(value.to_owned())
                        .text_color(rgb(theme::TEXT))
                        .font_weight(FontWeight::MEDIUM)
                } else {
                    app_strong_text(value.to_owned())
                })
                .child(app_muted_text("voting power")),
        )
        .when_some(qualifier, |this, qualifier| {
            this.child(
                app_muted_text(qualifier)
                    .text_size(px(11.0))
                    .whitespace_normal(),
            )
        })
}

pub(super) fn staking_account_header_compact(
    available_width: gpui::Pixels,
    table_compact: bool,
) -> bool {
    table_compact || available_width < STAKING_ACCOUNT_HEADER_STACK_BREAKPOINT
}

pub(super) fn render_staking_account_header(
    identity: gpui::Div,
    voting_power: gpui::Div,
    compact: bool,
    debug_prefix: Option<&str>,
) -> gpui::Div {
    let identity = identity.when(compact, |this| this.w_full().flex_initial());
    let identity = if let Some(prefix) = debug_prefix {
        let selector = format!("{prefix}-identity");
        identity.debug_selector(move || selector)
    } else {
        identity
    };
    let voting_power = if let Some(prefix) = debug_prefix {
        let selector = format!("{prefix}-metric-0");
        voting_power.debug_selector(move || selector)
    } else {
        voting_power
    };
    let voting_power = voting_power
        .flex_none()
        .when(compact, |this| this.w_full().items_end());
    if compact {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(identity)
            .child(voting_power)
    } else {
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .justify_between()
            .gap_3()
            .child(identity)
            .child(voting_power)
    }
}

pub(super) fn format_staking_amount(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
) -> String {
    if governance_contracts(chain_id).is_some_and(|contracts| contracts.governance_token == token) {
        let symbol = token_display_metadata(Some(registry), chain_id, &token)
            .map(|metadata| metadata.symbol)
            .or_else(|| {
                governance_contracts(chain_id).and_then(|contracts| {
                    contracts
                        .reward_tokens
                        .iter()
                        .find(|reward_token| reward_token.token == token)
                        .map(|reward_token| reward_token.symbol.to_owned())
                })
            })
            .unwrap_or_else(|| short_address(&token));
        format!("{} {symbol}", format_compact_rail_amount(amount))
    } else {
        format_token_amount_for_display(chain_id, token, amount, Some(registry))
    }
}

pub(super) fn format_reward_amount(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
) -> String {
    if governance_contracts(chain_id).is_some_and(|contracts| contracts.governance_token == token) {
        return format_compact_rail_amount(amount);
    }
    token_display_metadata(Some(registry), chain_id, &token).map_or_else(
        || amount.to_string(),
        |metadata| railgun_ui::format_token_amount(amount, metadata.decimals),
    )
}

pub(super) fn staking_control_id(
    actor_uuid: &str,
    action: &str,
    subject: impl std::fmt::Display,
) -> SharedString {
    SharedString::from(format!(
        "governance-staking-{action}-{actor_uuid}-{subject}"
    ))
}

fn compact_stake_id_preview(id: U256) -> String {
    let decimal = id.to_string();
    if decimal.len() <= 8 {
        return decimal;
    }
    format!("{}…{}", &decimal[..3], &decimal[decimal.len() - 2..])
}

fn stake_state_color(state: &str) -> u32 {
    match state {
        "Active" => theme::SUCCESS,
        "Unlocking" => theme::WARNING,
        "Claimable" => theme::INFO,
        _ => theme::TEXT_MUTED,
    }
}

pub(super) fn stake_table_row(
    stake: &StakePosition,
    actor: Address,
    inactive: bool,
    account_action_ready: bool,
    chain_id: u64,
    token: Address,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
    chain_time: Option<U256>,
) -> StakeTableRow {
    let state = match stake.state {
        StakeState::Active => "Active",
        StakeState::Unlocking => "Unlocking",
        StakeState::Claimable => "Claimable",
        StakeState::Claimed => "Claimed",
    };
    let state_detail = match stake.state {
        StakeState::Active | StakeState::Claimed => String::new(),
        StakeState::Unlocking if stake.locktime.is_zero() => {
            "Claimable date unavailable".to_owned()
        }
        StakeState::Unlocking => {
            let date = format_date_short(&stake.locktime);
            let remaining = chain_time
                .and_then(|now| stake.locktime.checked_sub(now))
                .and_then(|seconds| seconds.try_into().ok());
            remaining.map_or_else(
                || format!("Claimable after {date}"),
                |seconds: u64| {
                    format!(
                        "{} remaining · Claimable {date}",
                        format_compact_duration(Duration::from_secs(seconds)),
                    )
                },
            )
        }
        StakeState::Claimable if stake.locktime.is_zero() => "Unlocked".to_owned(),
        StakeState::Claimable => {
            format!("Unlocked · {}", format_date_short(&stake.locktime))
        }
    };
    let actions = match stake.state {
        StakeState::Active if stake.delegate == actor => {
            vec![StakeTableAction::Delegate, StakeTableAction::Unlock]
        }
        StakeState::Active => vec![StakeTableAction::Undelegate, StakeTableAction::Unlock],
        StakeState::Unlocking | StakeState::Claimed => vec![],
        StakeState::Claimable => vec![StakeTableAction::PrincipalClaim],
    };
    StakeTableRow {
        id: stake.id,
        amount: format_staking_amount(chain_id, token, stake.amount, registry),
        created: format_date_short(&stake.staketime),
        delegate: stake.delegate,
        delegate_label: if stake.delegate == actor {
            "Self-delegated".to_owned()
        } else {
            short_address(&stake.delegate)
        },
        state: state.to_owned(),
        state_detail,
        externally_delegated: stake.delegate != actor,
        actions,
        disabled: inactive || !account_action_ready,
    }
}

pub(super) fn staking_table_layout(available_width: gpui::Pixels) -> StakingTableLayout {
    let target_width = available_width.max(STAKING_TABLE_MIN_WIDTH);
    let compact = available_width < STAKING_TABLE_COMPACT_BREAKPOINT;
    let (kinds, widths) = if compact {
        let fixed_width = STAKING_TABLE_STAKE_WIDTH
            + STAKING_TABLE_AMOUNT_WIDTH
            + STAKING_TABLE_STATE_WIDTH
            + STAKING_TABLE_COMPACT_ACTIONS_WIDTH;
        let delegate_width =
            (target_width - fixed_width).max(STAKING_TABLE_COMPACT_DELEGATE_MIN_WIDTH);
        let widths = vec![
            STAKING_TABLE_STAKE_WIDTH,
            STAKING_TABLE_AMOUNT_WIDTH,
            delegate_width,
            STAKING_TABLE_STATE_WIDTH,
            STAKING_TABLE_COMPACT_ACTIONS_WIDTH,
        ];
        (
            vec![
                StakeTableColumnKind::Stake,
                StakeTableColumnKind::Amount,
                StakeTableColumnKind::Delegate,
                StakeTableColumnKind::State,
                StakeTableColumnKind::Actions,
            ],
            widths,
        )
    } else {
        let fixed_width = STAKING_TABLE_STAKE_WIDTH
            + STAKING_TABLE_AMOUNT_WIDTH
            + STAKING_TABLE_STATE_WIDTH
            + STAKING_TABLE_ACTIONS_WIDTH;
        let delegate_width = (target_width - fixed_width).max(px(0.0));
        let widths = vec![
            STAKING_TABLE_STAKE_WIDTH,
            STAKING_TABLE_AMOUNT_WIDTH,
            delegate_width,
            STAKING_TABLE_STATE_WIDTH,
            STAKING_TABLE_ACTIONS_WIDTH,
        ];
        (
            vec![
                StakeTableColumnKind::Stake,
                StakeTableColumnKind::Amount,
                StakeTableColumnKind::Delegate,
                StakeTableColumnKind::State,
                StakeTableColumnKind::Actions,
            ],
            widths,
        )
    };
    let columns = kinds
        .into_iter()
        .zip(widths)
        .map(|(kind, width)| {
            let (key, name) = match kind {
                StakeTableColumnKind::Stake => ("stake", "Stake"),
                StakeTableColumnKind::Amount => ("amount", "Amount"),
                StakeTableColumnKind::Delegate => ("delegate", "Delegate"),
                StakeTableColumnKind::State => ("state", "State"),
                StakeTableColumnKind::Actions => ("actions", "Actions"),
            };
            let column = Column::new(key, name).width(width).movable(false);
            let column = if compact && kind != StakeTableColumnKind::Actions {
                column.paddings(gpui::Edges::all(px(2.0)))
            } else if kind == StakeTableColumnKind::Actions {
                column.paddings(gpui::Edges {
                    top: px(4.0),
                    right: px(12.0),
                    bottom: px(4.0),
                    left: px(4.0),
                })
            } else {
                column
            };
            (kind, column)
        })
        .collect();
    StakingTableLayout { compact, columns }
}

impl StakeTableDelegate {
    pub(super) fn new(
        root: WeakEntity<WalletRoot>,
        actor_uuid: String,
        actor: Address,
        rows: Vec<StakeTableRow>,
        layout: StakingTableLayout,
    ) -> Self {
        Self {
            root,
            actor_uuid,
            actor,
            rows: Arc::from(rows),
            compact: layout.compact,
            columns: layout.columns,
            pending_keyboard_focus: None,
        }
    }

    pub(super) fn set_rows(&mut self, rows: Vec<StakeTableRow>) -> bool {
        let rows: Arc<[StakeTableRow]> = Arc::from(rows);
        if self
            .pending_keyboard_focus
            .is_some_and(|id| !rows.iter().any(|row| row.id == id))
        {
            self.pending_keyboard_focus = None;
        }
        if self.rows.as_ref() == rows.as_ref() {
            return false;
        }
        self.rows = rows;
        true
    }

    pub(super) fn set_layout(&mut self, layout: &StakingTableLayout) -> bool {
        if self.compact != layout.compact
            || self.columns.len() != layout.columns.len()
            || self.columns.iter().zip(&layout.columns).any(
                |((kind, column), (next_kind, next_column))| {
                    kind != next_kind || column.width != next_column.width
                },
            )
        {
            if self.compact && !layout.compact {
                self.pending_keyboard_focus = None;
            }
            self.compact = layout.compact;
            self.columns.clone_from(&layout.columns);
            true
        } else {
            false
        }
    }
}

pub(super) fn render_staking_table_row(row_ix: usize) -> gpui::Stateful<gpui::Div> {
    div().id(("row", row_ix)).relative().child(
        div()
            .id(("row-pointer-guard", row_ix))
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .on_click(|_event, _window, cx| cx.stop_propagation()),
    )
}

impl TableDelegate for StakeTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].1.clone()
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("header")
            .absolute()
            .h(px(0.0))
            .max_h(px(0.0))
            .border_0()
            .overflow_hidden()
    }

    fn render_th(
        &mut self,
        _col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        div().h(px(0.0)).max_h(px(0.0)).overflow_hidden()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        render_staking_table_row(row_ix)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let table = cx.entity();
        match self.columns[col_ix].0 {
            StakeTableColumnKind::Stake => {
                let key = (self.actor_uuid.clone(), row.id);
                let details_root = self.root.clone();
                let details_root_for_keyboard = self.root.clone();
                let details_key = key.clone();
                let details_key_for_keyboard = key.clone();
                let details_key_for_content = key.clone();
                let table_for_keyboard = table.clone();
                let table_for_open_change = table;
                let details_open = self
                    .root
                    .read_with(cx, |root, _| {
                        root.governance.compact_position_details.as_ref() == Some(&key)
                    })
                    .unwrap_or(false);
                let focus_on_materialize =
                    if details_open && self.pending_keyboard_focus == Some(row.id) {
                        self.pending_keyboard_focus = None;
                        true
                    } else {
                        false
                    };
                let delegate = row.delegate.to_checksum(None);
                let delegate_for_copy = delegate.clone();
                let full_id = row.id.to_string();
                let amount = row.amount.clone();
                let created = row.created.clone();
                let state = row.state.clone();
                let state_detail = row.state_detail.clone();
                let trigger = Button::new(staking_control_id(
                    &self.actor_uuid,
                    "stake-details-trigger",
                    row.id,
                ))
                .text()
                .xsmall()
                .child(
                    app_button_label(format!("#{}", compact_stake_id_preview(row.id)))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .font_family(APP_MONO_FONT_FAMILY),
                )
                .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                        if let Some(root) = details_root_for_keyboard.upgrade() {
                            let currently_open =
                                root.read(cx).governance.compact_position_details.as_ref()
                                    == Some(&details_key_for_keyboard);
                            table_for_keyboard.update(cx, |table, _| {
                                table.delegate_mut().pending_keyboard_focus =
                                    (!currently_open).then_some(details_key_for_keyboard.1);
                            });
                            root.update(cx, |root, cx| {
                                let next = if root.governance.compact_position_details.as_ref()
                                    == Some(&details_key_for_keyboard)
                                {
                                    None
                                } else {
                                    Some(details_key_for_keyboard.clone())
                                };
                                root.set_compact_position_details(next, cx);
                            });
                        }
                        cx.stop_propagation();
                    }
                });
                let popover = Popover::new(staking_control_id(
                    &self.actor_uuid,
                    "stake-details-popover",
                    row.id,
                ))
                .open(details_open)
                .on_open_change(move |open, _window, cx| {
                    table_for_open_change.update(cx, |table, _| {
                        table.delegate_mut().pending_keyboard_focus = None;
                    });
                    if let Some(root) = details_root.upgrade() {
                        root.update(cx, |root, cx| {
                            let next = if *open {
                                Some(details_key.clone())
                            } else if root.governance.compact_position_details.as_ref()
                                == Some(&details_key)
                            {
                                None
                            } else {
                                root.governance.compact_position_details.clone()
                            };
                            root.set_compact_position_details(next, cx);
                        });
                    }
                })
                .trigger(trigger)
                .content(move |popover_state, window, cx| {
                    let focus_handle = popover_state.focus_handle(cx);
                    if focus_on_materialize {
                        focus_handle.focus(window, cx);
                    }
                    let full_id = full_id.clone();
                    let amount = amount.clone();
                    let created = created.clone();
                    let delegate = delegate.clone();
                    let delegate_for_copy = delegate_for_copy.clone();
                    let state = state.clone();
                    let state_detail = state_detail.clone();
                    let details_key = details_key_for_content.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .w(px(300.0))
                        .max_w(px(300.0))
                        .child(app_muted_text("Stake").text_size(px(11.0)))
                        .child(app_text(format!("#{full_id}")).font_family(APP_MONO_FONT_FAMILY))
                        .child(app_muted_text("Amount").text_size(px(11.0)))
                        .child(app_text(amount).whitespace_normal())
                        .child(app_muted_text("Created").text_size(px(11.0)))
                        .child(app_text(created).whitespace_normal())
                        .child(app_muted_text("Delegate").text_size(px(11.0)))
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap_1()
                                .child(
                                    app_text(delegate)
                                        .font_family(APP_MONO_FONT_FAMILY)
                                        .whitespace_normal()
                                        .min_w(px(0.0))
                                        .flex_1(),
                                )
                                .child(clipboard_with_toast(
                                    staking_control_id(
                                        &details_key.0,
                                        "stake-details-delegate-copy",
                                        details_key.1,
                                    ),
                                    delegate_for_copy,
                                )),
                        )
                        .child(app_muted_text("State").text_size(px(11.0)))
                        .child(app_text(state))
                        .when(!state_detail.is_empty(), |this| {
                            this.child(app_muted_text(state_detail).whitespace_normal())
                        })
                });
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(popover)
                    .into_any_element()
            }
            StakeTableColumnKind::Amount => div()
                .h_full()
                .flex()
                .items_center()
                .child(
                    app_strong_text(row.amount.clone()).when(self.compact, gpui::Styled::truncate),
                )
                .into_any_element(),
            StakeTableColumnKind::Delegate => {
                let copy_id = staking_control_id(&self.actor_uuid, "delegate-copy", row.id);
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_muted_text(row.delegate_label.clone()).truncate())
                    .when(row.externally_delegated, |this| {
                        this.child(clipboard_with_toast(
                            copy_id,
                            format!("{:#x}", row.delegate),
                        ))
                    })
                    .into_any_element()
            }
            StakeTableColumnKind::State => div()
                .h_full()
                .flex()
                .items_center()
                .gap_1()
                .min_w(px(0.0))
                .child(div().flex_none().child(app_status_tag(
                    row.state.clone(),
                    stake_state_color(&row.state),
                )))
                .when(!row.state_detail.is_empty(), |this| {
                    let detail_id = staking_control_id(&self.actor_uuid, "state-detail", row.id);
                    let detail = row.state_detail.clone();
                    let tooltip_detail = detail.clone();
                    this.child(
                        div()
                            .id(detail_id)
                            .flex_1()
                            .min_w(px(0.0))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_detail.clone()).build(window, cx)
                            })
                            .child(app_muted_text(detail).text_size(px(11.0)).truncate()),
                    )
                })
                .into_any_element(),
            StakeTableColumnKind::Actions => {
                let mut actions = div().h_full().flex().items_center().justify_end().gap_1();
                for action in &row.actions {
                    let (label, discriminator, kind) = match action {
                        StakeTableAction::Delegate => (
                            "Delegate",
                            "delegate",
                            StakingActionKind::Delegate { stake_id: row.id },
                        ),
                        StakeTableAction::Undelegate => (
                            "Undelegate",
                            "undelegate",
                            StakingActionKind::Undelegate { stake_id: row.id },
                        ),
                        StakeTableAction::Unlock => {
                            let label = if row.externally_delegated && !self.compact {
                                "Unlock · 2 steps"
                            } else {
                                "Unlock"
                            };
                            (
                                label,
                                "unlock",
                                StakingActionKind::Unlock { stake_id: row.id },
                            )
                        }
                        StakeTableAction::PrincipalClaim => (
                            "Claim principal",
                            "principal-claim",
                            StakingActionKind::PrincipalClaim { stake_id: row.id },
                        ),
                    };
                    let root = self.root.clone();
                    let actor = self.actor;
                    let actor_uuid = self.actor_uuid.clone();
                    let id = staking_control_id(&actor_uuid, discriminator, row.id);
                    let is_primary = matches!(action, StakeTableAction::PrincipalClaim);
                    let button = app_button_base(id).small().disabled(row.disabled);
                    let button = if is_primary {
                        button.primary()
                    } else {
                        button.outline()
                    };
                    let button = button.child(label).on_click(move |_event, window, cx| {
                        if let Some(root) = root.upgrade() {
                            root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    actor_uuid.as_str(),
                                    actor,
                                    kind.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }
                    });
                    actions = actions.child(button);
                }
                if row.state == "Unlocking" {
                    actions = actions.child(
                        app_button_base(staking_control_id(
                            &self.actor_uuid,
                            "claim-principal",
                            row.id,
                        ))
                        .outline()
                        .small()
                        .disabled(true)
                        .child("Claim principal"),
                    );
                }
                actions.into_any_element()
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        app_muted_text("No staking positions yet.").into_any_element()
    }
}

pub(super) fn render_reward_row(
    chain_id: u64,
    symbol: &str,
    token: Address,
    reward: Option<&RewardView>,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
    anchor_cache: &TokenAnchorRateCache,
) -> gpui::Div {
    let metadata = token_display_metadata(Some(registry), chain_id, &token);
    let display_symbol = metadata
        .as_ref()
        .map_or_else(|| symbol.to_owned(), |info| info.symbol.clone());
    let icon_path = metadata.and_then(|info| info.icon_path);
    let display_amount = |amount| format_reward_amount(chain_id, token, amount, registry);
    let (amount, detail) = match reward {
        None => ("Loading...".to_owned(), "Calculation pending".to_owned()),
        Some(RewardView::Zero) => (
            display_amount(U256::ZERO),
            "No completed unclaimed intervals".to_owned(),
        ),
        Some(RewardView::Positive { amount, .. }) => (display_amount(*amount), String::new()),
        Some(RewardView::Unavailable(error)) => ("Unavailable".to_owned(), error.to_string()),
    };
    let usd_label = reward_usd_label(chain_id, token, reward, anchor_cache);
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .min_w(px(0.0))
        .py_2()
        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            token_label_row(SharedString::from(display_symbol), icon_path, px(16.0))
                .w(px(110.0))
                .truncate(),
        )
        .child(
            div()
                .w(px(180.0))
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    if reward.is_none() {
                        app_shimmer_text(amount)
                            .text_color(rgb(theme::TEXT))
                            .font_weight(FontWeight::MEDIUM)
                    } else {
                        app_strong_text(amount)
                    }
                    .truncate(),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_w(px(0.0))
                .when_some(usd_label, |column, usd_label| {
                    column.child(app_muted_text(usd_label).whitespace_nowrap())
                })
                .when(!detail.is_empty(), |column| {
                    column.child(app_muted_text(detail).truncate())
                }),
        )
}

pub(in crate::root) fn unlock_period_label(seconds: U256) -> String {
    let day = U256::from(86_400u64);
    if !seconds.is_zero() && seconds % day == U256::ZERO {
        let days = seconds / day;
        return format!(
            "{} day{}",
            days,
            if days == U256::from(1) { "" } else { "s" }
        );
    }
    u64::try_from(seconds).ok().map_or_else(
        || format!("{seconds} seconds"),
        |seconds| format_compact_duration(Duration::from_secs(seconds)),
    )
}

/// Gas in the compact form the fee sub-lines use.
pub(super) fn compact_gas(gas: u64) -> String {
    format!("{}k", gas.saturating_add(500) / 1_000)
}

/// The estimate's fee per gas in gwei, to one decimal.
fn gwei_label(fee_per_gas: u128) -> String {
    let whole = fee_per_gas / 1_000_000_000;
    if whole >= 10 {
        return format!("{whole} gwei");
    }
    let hundredths = fee_per_gas % 1_000_000_000 / 10_000_000;
    format!("{whole}.{hundredths:02} gwei")
}

/// The gas sub-line under the selection fee: the single call, or the split and its per-step gas.
pub(super) fn reward_selection_gas_summary(
    steps: &[wallet_ops::RewardClaimStep],
    fee_per_gas: u128,
) -> String {
    if let [step] = steps {
        return format!(
            "{} gas · {}",
            compact_gas(step.estimated_gas),
            gwei_label(fee_per_gas)
        );
    }
    format!(
        "{} transactions · {} gas",
        steps.len(),
        steps
            .iter()
            .map(|step| compact_gas(step.estimated_gas))
            .collect::<Vec<_>>()
            .join(" + ")
    )
}

/// Reward value minus claim fee, and whether the fee was the larger of the two.
pub(super) fn reward_net_label(reward_usd: U256, fee_usd: U256) -> (String, bool) {
    if fee_usd > reward_usd {
        (
            format!("-{}", format_usd_micro_value(fee_usd - reward_usd)),
            true,
        )
    } else {
        (format_usd_micro_value(reward_usd - fee_usd), false)
    }
}
