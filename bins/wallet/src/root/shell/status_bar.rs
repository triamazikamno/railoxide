#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Status bar center (sync progress) and right chips for the selected wallet on its
    /// selected chain, on every workspace.
    pub(in crate::root) fn render_wallet_status(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
    ) -> (Option<gpui::Div>, Vec<gpui::AnyElement>) {
        let Some(state) = self
            .selected_evm_chain()
            .and_then(|chain_id| self.chain_states.get(&chain_id))
            .filter(|state| state.renders_table())
        else {
            return (None, Vec::new());
        };
        let counts = self.wallet_status_counts(
            state.snapshot().map(AsRef::as_ref),
            state.ppoi_workflow_status(),
        );
        let center = match state {
            ChainUtxoState::Loading { .. } => Some(sync_status_center(
                SyncStatusContext::Loading,
                state.progress(),
            )),
            ChainUtxoState::Syncing { .. } => Some(sync_status_center(
                SyncStatusContext::Syncing,
                state.progress(),
            )),
            ChainUtxoState::Ready { .. } => ready_status_center(counts),
            ChainUtxoState::Idle | ChainUtxoState::Error { .. } => None,
        };
        (
            center,
            self.render_wallet_status_chips(root, state, counts, dialog_open),
        )
    }

    pub(super) fn wallet_status_counts(
        &self,
        snapshot: Option<&wallet_ops::ListUtxosOutput>,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
    ) -> WalletStatusCounts {
        let Some(snapshot) = snapshot else {
            return WalletStatusCounts {
                ppoi_workflow_status,
                ..WalletStatusCounts::default()
            };
        };
        let assets = format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        );
        WalletStatusCounts {
            pending_incoming_outputs: snapshot.utxos.iter().filter(|row| row.pending_new).count(),
            pending_outgoing_outputs: snapshot
                .utxos
                .iter()
                .filter(|row| row.pending_spent || row.local_pending_spent)
                .count(),
            pending_poi_assets: assets
                .iter()
                .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
                .count(),
            recoverable_poi_outputs: recoverable_poi_candidate_count(snapshot),
            blocked_shield_outputs: blocked_shield_rescue_display_rows(
                snapshot,
                &self.blocked_shield_rescue_rows,
                &self.blocked_shield_refunds_in_flight,
            )
            .len(),
            ppoi_workflow_status,
        }
    }

    pub(super) fn ppoi_status_for_state(
        &self,
        state: &ChainUtxoState,
        counts: WalletStatusCounts,
    ) -> PresenceStatus {
        ppoi_presence_status(
            state.poi_refreshing(),
            state.poi_refresh_session().is_some(),
            self.poi_read_source.is_indexed_artifacts(),
            self.selected_chain_poi_artifact_progress(),
            counts,
        )
    }

    pub(super) fn balances_status_for_state(&self, state: &ChainUtxoState) -> PresenceStatus {
        let Some(block_time) = self
            .effective_chain_configs
            .get(self.last_evm_chain)
            .and_then(|chain| chain.block_time)
        else {
            return PresenceStatus::Unknown;
        };
        balances_presence_status(
            state.is_syncing(),
            matches!(state, ChainUtxoState::Ready { .. }),
            state.sync_tip(),
            block_time,
            now_epoch_secs(),
        )
    }

    fn render_wallet_status_chips(
        &self,
        root: &Entity<Self>,
        state: &ChainUtxoState,
        counts: WalletStatusCounts,
        dialog_open: bool,
    ) -> Vec<gpui::AnyElement> {
        let ppoi_status = self.ppoi_status_for_state(state, counts);
        let balances_status = self.balances_status_for_state(state);
        let mut chips = Vec::new();

        if counts.ppoi_status_count() > 0 {
            chips.push(Self::render_ppoi_status_indicator(
                root,
                ppoi_status,
                counts,
                "PPOI",
                dialog_open,
            ));
        } else {
            chips.push(
                render_ppoi_status_hover_target(root, "wallet-status-ppoi")
                    .child(status_presence_text("PPOI", ppoi_status))
                    .into_any_element(),
            );
        }
        chips.push(
            render_balances_status_hover_target(root, "wallet-status-balances")
                .child(status_presence_text("Balances", balances_status))
                .into_any_element(),
        );
        chips
    }

    fn render_ppoi_status_indicator(
        root: &Entity<Self>,
        status: PresenceStatus,
        counts: WalletStatusCounts,
        label: &'static str,
        dialog_open: bool,
    ) -> gpui::AnyElement {
        let details_root = root.clone();
        render_ppoi_status_hover_target(root, "wallet-status-ppoi-hover")
            // Above a dialog the chip keeps its hover details but opens nothing.
            .when(!dialog_open, |this| {
                this.cursor_pointer().on_click(move |_event, window, cx| {
                    details_root.update(cx, |root, cx| {
                        root.open_private_pending_status_dialog(window, cx);
                    });
                })
            })
            .child(
                Badge::new()
                    .count(counts.ppoi_status_count())
                    .color(rgb(ppoi_attention_badge_color(counts)))
                    .child(
                        status_presence_text(label, status)
                            .pr(px(12.0))
                            .into_any_element(),
                    ),
            )
            .into_any_element()
    }
}
