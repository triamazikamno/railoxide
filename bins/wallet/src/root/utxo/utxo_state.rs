use super::*;

impl WalletRoot {
    pub(in crate::root) fn sync_utxo_table(&mut self, cx: &mut Context<'_, Self>) {
        let (mut rows, snapshot) = match self.chain_states.get(&self.last_evm_chain) {
            Some(state) => {
                let snapshot = state.snapshot().cloned();
                let rows = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                    display_rows_from_output(
                        snapshot,
                        self.tx_search_query.as_ref(),
                        self.show_spent_utxos,
                    )
                });
                (rows, snapshot)
            }
            _ => (Vec::new(), None),
        };
        if let Some(snapshot) = snapshot.as_ref() {
            self.prune_blocked_shield_rescue_rows(snapshot);
            apply_blocked_shield_rescue_rows(
                &mut rows,
                &self.blocked_shield_rescue_rows,
                &self.blocked_shield_refunds_in_flight,
            );
        }
        // The debug UI fixture shows its synthesized rows. No release build has any.
        if let Some(mut fixture_rows) =
            crate::root::private_swap::ui_fixture_utxo_rows(self.last_evm_chain)
        {
            let query = self.tx_search_query.as_ref();
            fixture_rows.retain(|row| {
                row.source_tx_hash.contains(query)
                    || row
                        .spent_tx_hash
                        .as_ref()
                        .is_some_and(|hash| hash.contains(query))
            });
            rows = fixture_rows;
        }
        let poi_refreshing = self
            .chain_states
            .get(&self.last_evm_chain)
            .is_some_and(ChainUtxoState::poi_refreshing);
        let poi_retry_session_available = self
            .chain_states
            .get(&self.last_evm_chain)
            .and_then(ChainUtxoState::poi_refresh_session)
            .is_some();
        self.executor_locked_note_count = self
            .selected_chain_session()
            .and_then(|session| session.note_locks().ok())
            .map_or(0, |locks| locks.executor_note_count());
        let finality_context = self.utxo_finality_context();
        self.utxo_table.update(cx, |state, cx| {
            state.delegate_mut().set_rows(
                rows,
                poi_refreshing,
                poi_retry_session_available,
                finality_context,
            );
            cx.notify();
        });
        // Activity's received rows and private statuses read the same snapshot.
        self.refresh_wallet_activity(cx);
    }

    fn utxo_finality_context(&self) -> UtxoFinalityContext {
        // The debug UI fixture's rows count from its own head. No release build has one.
        if let Some(context) = crate::root::private_swap::ui_fixture_utxo_finality_context() {
            return context;
        }
        let sync_tip = self
            .chain_states
            .get(&self.last_evm_chain)
            .and_then(ChainUtxoState::sync_tip);
        UtxoFinalityContext::new(
            sync_tip.and_then(|tip| tip.head_block),
            sync_tip.and_then(|tip| tip.safe_head_block),
            self.effective_chain_configs
                .get(self.last_evm_chain)
                .map(|config| config.finality_depth),
        )
    }

    pub(in crate::root) fn sync_utxo_finality_context(&self, cx: &mut Context<'_, Self>) {
        let context = self.utxo_finality_context();
        self.utxo_table.update(cx, |state, cx| {
            if state.delegate_mut().set_finality_context(context) {
                cx.notify();
            }
        });
    }

    pub(in crate::root) fn sync_utxo_poi_refreshing(
        &self,
        poi_refreshing: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.utxo_table.update(cx, |state, cx| {
            if state.delegate_mut().set_poi_refreshing(poi_refreshing) {
                cx.notify();
            }
        });
    }

    pub(super) fn set_spent_visibility(&mut self, show_spent: bool, cx: &mut Context<'_, Self>) {
        if self.show_spent_utxos == show_spent {
            return;
        }
        self.show_spent_utxos = show_spent;
        self.sync_utxo_table(cx);
        cx.notify();
    }

    pub(super) fn begin_clear_local_pending_spent_confirmation(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.local_pending_spent_clear_confirming = true;
        cx.notify();
    }

    pub(super) fn cancel_clear_local_pending_spent_confirmation(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.local_pending_spent_clear_confirming = false;
        cx.notify();
    }

    pub(super) fn clear_local_pending_spent_locks(&mut self, cx: &mut Context<'_, Self>) {
        let Some(session) = self.selected_chain_session() else {
            self.local_pending_spent_clear_confirming = false;
            cx.notify();
            return;
        };
        self.local_pending_spent_clear_confirming = false;
        let clear = self
            .runtime
            .spawn(async move { session.clear_local_pending_spent().await });
        cx.spawn(async move |this, cx| {
            let changed = clear.await.unwrap_or(false);
            let _ = this.update(cx, |root, cx| {
                if changed {
                    root.sync_utxo_table(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn retry_poi_submissions(
        session: Option<Arc<wallet_ops::WalletSession>>,
        cx: &App,
    ) {
        let Some(session) = session else {
            return;
        };
        cx.spawn(async move |_cx| {
            session.refresh_poi_statuses().await;
        })
        .detach();
    }

    pub(in crate::root) fn selected_chain_session(&self) -> Option<Arc<wallet_ops::WalletSession>> {
        self.chain_states
            .get(&self.selected_evm_chain()?)
            .and_then(ChainUtxoState::poi_refresh_session)
    }

    fn prune_blocked_shield_rescue_rows(&mut self, snapshot: &ListUtxosOutput) {
        let current_ids: BTreeSet<_> = snapshot
            .utxos
            .iter()
            .filter(|row| row.blocked_shield_rescue.is_some())
            .filter_map(blocked_shield_rescue_utxo_id_from_output)
            .collect();
        let active_ids: BTreeSet<_> = snapshot
            .utxos
            .iter()
            .filter_map(active_blocked_shield_rescue_utxo_id_from_output)
            .collect();
        self.blocked_shield_rescue_rows
            .retain(|utxo_id, _| active_ids.contains(utxo_id));
        self.blocked_shield_refunds_in_flight
            .retain(|utxo_id| current_ids.contains(utxo_id));
    }

    pub(in crate::root) fn invalidate_blocked_shield_rescue_rows(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if self.blocked_shield_rescue_rows.is_empty() {
            return;
        }
        self.blocked_shield_rescue_rows.clear();
        self.blocked_shield_rescue_lookup_generation =
            self.blocked_shield_rescue_lookup_generation.wrapping_add(1);
        self.sync_utxo_table(cx);
    }

    pub(super) const fn next_blocked_shield_rescue_lookup_generation(&mut self) -> u64 {
        self.blocked_shield_rescue_lookup_generation =
            self.blocked_shield_rescue_lookup_generation.wrapping_add(1);
        self.blocked_shield_rescue_lookup_generation
    }

    /// The display row of the active blocked Shield `utxo_id`, with its row state and an
    /// in-flight refund applied, as the table shows it.
    pub(super) fn active_blocked_shield_rescue_display_row(
        &self,
        utxo_id: BlockedShieldRescueUtxoId,
    ) -> Option<UtxoDisplayRow> {
        let snapshot = self.chain_states.get(&self.last_evm_chain)?.snapshot()?;
        let mut row = snapshot
            .utxos
            .iter()
            .find(|row| active_blocked_shield_rescue_utxo_id_from_output(row) == Some(utxo_id))
            .map(|row| display_row_from_utxo(snapshot.chain_id, row))?;
        apply_blocked_shield_rescue_rows(
            std::slice::from_mut(&mut row),
            &self.blocked_shield_rescue_rows,
            &self.blocked_shield_refunds_in_flight,
        );
        Some(row)
    }
}
