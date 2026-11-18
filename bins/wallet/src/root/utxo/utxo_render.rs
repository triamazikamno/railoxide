use super::*;

impl WalletRoot {
    pub(in crate::root) fn focus_utxo_table_if_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.focus_utxo_table_on_render
            || !should_focus_utxo_table(
                self.active_activity,
                self.active_wallet_tab,
                self.chain_states.get(&self.last_evm_chain),
            )
        {
            return;
        }
        if self
            .tx_search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
        {
            return;
        }

        self.utxo_table.read(cx).focus_handle(cx).focus(window, cx);
        self.focus_utxo_table_on_render = false;
    }

    pub(in crate::root) fn render_utxo_body(
        &self,
        root: &Entity<Self>,
        window: &Window,
    ) -> gpui::Div {
        if self.view_session.is_none() {
            return centered_message("Choose a wallet to view UTXOs");
        }
        // The debug UI fixture's rows show in the table whatever the chain's state. No release
        // build has any.
        let fixture_rows = crate::root::private_swap::ui_fixture_shows_utxos();
        match self.chain_states.get(&self.last_evm_chain) {
            Some(ChainUtxoState::Error { message, .. }) if !fixture_rows => div()
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .child(self.render_chain_error_body(root, message.as_ref())),
                ),
            Some(ChainUtxoState::Ready {
                snapshot, session, ..
            }) if snapshot.utxo_count == 0 && !fixture_rows => div()
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .gap_2()
                .child(self.render_utxo_controls(root))
                .child(centered_message(format!(
                    "No UTXOs found. Synced from block {}.",
                    session.start_block
                ))),
            state if fixture_rows || state.is_some_and(ChainUtxoState::renders_table) => div()
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .gap_2()
                .child(self.render_utxo_controls(root))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .on_mouse_down(MouseButton::Left, {
                            let table = self.utxo_table.clone();
                            move |_event, window, cx| {
                                table.update(cx, |table, cx| {
                                    table.focus_handle(cx).focus(window, cx);
                                });
                            }
                        })
                        .on_action(window.listener_for(root, Self::on_action_utxo_page_up))
                        .on_action(window.listener_for(root, Self::on_action_utxo_page_down))
                        .on_action(window.listener_for(root, Self::on_action_utxo_home))
                        .on_action(window.listener_for(root, Self::on_action_utxo_end))
                        .child(DataTable::new(&self.utxo_table).large()),
                ),
            _ => centered_message("Select a chain to load UTXOs"),
        }
    }

    fn render_utxo_controls(&self, root: &Entity<Self>) -> impl IntoElement {
        let search_active = !self.tx_search_query.is_empty();
        let state = self.chain_states.get(&self.last_evm_chain);
        let snapshot = state.and_then(ChainUtxoState::snapshot);
        let local_pending_spent_count =
            snapshot.map_or(0, |snapshot| snapshot.local_pending_spent_count);
        let ppoi_workflow_status = state.map_or_else(
            WalletPpoiWorkflowStatus::default,
            ChainUtxoState::ppoi_workflow_status,
        );
        let owned_ppoi_retry_candidates = snapshot.map_or(0, |snapshot| {
            recoverable_poi_candidate_count(snapshot.as_ref())
        });
        let clear_search_input = self.tx_search_input.clone();
        let clear_search_table = self.utxo_table.clone();
        let search_input = app_input(&self.tx_search_input)
            .small()
            .when(search_active, |input| {
                input.suffix(
                    app_button_base("wallet-search-clear")
                        .ghost()
                        .xsmall()
                        .accessibility_label("Clear search")
                        .tooltip("Clear search")
                        .icon(IconName::Close)
                        .on_click(move |_event, window, cx| {
                            clear_search_input.update(cx, |input, cx| {
                                input.replace_all("", window, cx);
                            });
                            clear_search_table.update(cx, |table, cx| {
                                table.focus_handle(cx).focus(window, cx);
                            });
                        }),
                )
            });
        let spent_toggle_root = root.clone();
        let spent_toggle = Checkbox::new("wallet-toggle-spent-utxos")
            .label("Show spent")
            .checked(self.show_spent_utxos)
            .xsmall()
            .disabled(search_active)
            .opacity(if search_active { 0.45 } else { 1.0 })
            .on_click(move |checked, _window, cx| {
                let checked = *checked;
                spent_toggle_root.update(cx, |root, cx| {
                    root.set_spent_visibility(checked, cx);
                });
            });
        let poi_refreshing = state.is_some_and(ChainUtxoState::poi_refreshing);
        let poi_refresh_session = state.and_then(ChainUtxoState::poi_refresh_session);
        let poi_retry_root = root.clone();
        let poi_retry_button = app_button(
            "wallet-retry-poi-recovery",
            poi_retry_button_label(poi_refreshing),
        )
            .outline()
            .small()
            .disabled(poi_refresh_session.is_none())
            .tooltip("PPOI submission is normally automatic. Retry also processes recipient and broadcaster-fee outputs created by this wallet.")
            .on_click(move |_event, _window, cx| {
                poi_retry_root.update(cx, |root, cx| {
                    Self::retry_poi_submissions(root.selected_chain_session(), cx);
                });
            });

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .when(local_pending_spent_count > 0, |this| {
                this.child(
                    self.render_local_pending_spent_summary(
                        root.clone(),
                        local_pending_spent_count,
                    ),
                )
            })
            .children(self.render_executor_lock_summary(root))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_start()
                    .gap_2()
                    .child(div().w(px(280.0)).child(search_input))
                    .child(spent_toggle)
                    .child(div().flex_1())
                    .when(
                        global_poi_retry_available(
                            poi_refresh_session.is_some(),
                            poi_refreshing,
                            ppoi_workflow_status.needs_attention,
                            owned_ppoi_retry_candidates,
                        ),
                        |this| this.child(poi_retry_button),
                    ),
            )
    }

    fn render_local_pending_spent_summary(
        &self,
        root: Entity<Self>,
        count: usize,
    ) -> impl IntoElement {
        let begin_root = root.clone();
        let cancel_root = root.clone();
        let clear_root = root;
        local_pending_spent_card(
            "wallet",
            local_pending_spent_summary(count),
            self.local_pending_spent_clear_confirming,
            move |_window, cx| {
                begin_root.update(cx, |root, cx| {
                    root.begin_clear_local_pending_spent_confirmation(cx);
                });
            },
            move |_window, cx| {
                cancel_root.update(cx, |root, cx| {
                    root.cancel_clear_local_pending_spent_confirmation(cx);
                });
            },
            move |_window, cx| {
                clear_root.update(cx, |root, cx| {
                    root.clear_local_pending_spent_locks(cx);
                });
            },
        )
    }

    fn on_action_utxo_page_up(
        &mut self,
        _: &UtxoPageUp,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.navigate_utxo_table(UtxoNavigation::PageUp, cx);
    }

    fn on_action_utxo_page_down(
        &mut self,
        _: &UtxoPageDown,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.navigate_utxo_table(UtxoNavigation::PageDown, cx);
    }

    fn on_action_utxo_home(&mut self, _: &UtxoHome, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.navigate_utxo_table(UtxoNavigation::Home, cx);
    }

    fn on_action_utxo_end(&mut self, _: &UtxoEnd, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.navigate_utxo_table(UtxoNavigation::End, cx);
    }

    fn navigate_utxo_table(&self, navigation: UtxoNavigation, cx: &mut Context<'_, Self>) {
        if !should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&self.last_evm_chain),
        ) {
            return;
        }

        self.utxo_table.update(cx, |table, cx| {
            let rows_count = table.delegate().rows_count(cx);
            if rows_count == 0 {
                return;
            }

            let visible_rows = table.visible_range().rows().clone();
            let page_size = visible_rows.len().saturating_sub(1).max(1);
            let last_row = rows_count.saturating_sub(1);
            let selected_row = table.selected_row();
            let target_row = match navigation {
                UtxoNavigation::Home => 0,
                UtxoNavigation::End => last_row,
                UtxoNavigation::PageUp => selected_row
                    .unwrap_or(visible_rows.start)
                    .saturating_sub(page_size),
                UtxoNavigation::PageDown => selected_row
                    .unwrap_or_else(|| visible_rows.end.saturating_sub(1))
                    .saturating_add(page_size)
                    .min(last_row),
            };

            table.set_selected_row(target_row, cx);
        });
    }
}
