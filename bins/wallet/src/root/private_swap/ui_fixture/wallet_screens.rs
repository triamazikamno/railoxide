#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// In a `refund:` mode, open the refund dialog of its blocked Shield in place of Stealth
    /// accounts, the first time they are opened. `true` when the dialog opened.
    pub(in crate::root) fn open_ui_fixture_refund_dialog(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        static OPENED: AtomicBool = AtomicBool::new(false);
        // Without a state there is no dialog to open: another mode, or `refund:stealth`
        // without its staged accounts.
        if refund_dialog_state(REFUND_UTXO).is_none() || OPENED.swap(true, Ordering::Relaxed) {
            return false;
        }
        Self::open_blocked_shield_refund_dialog(REFUND_UTXO, window, cx);
        true
    }

    /// In an `update-check:` mode, open the update check dialog on its canned result the
    /// first time the locked-vault screen renders.
    pub(in crate::root) fn open_ui_fixture_update_check(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        static OPENED: AtomicBool = AtomicBool::new(false);
        if !matches!(mode(), Some(Mode::UpdateCheck(_))) || OPENED.swap(true, Ordering::Relaxed) {
            return;
        }
        self.open_update_check_dialog(window, cx);
        // The fixture's check shows the canned result and sends nothing.
        self.start_update_check(cx);
    }

    /// In a `broadcaster-progress:` mode, open the progress dialog of a synthesized Send via
    /// broadcaster of `asset` in place of its form, held at the mode's state; in the
    /// `broadcaster-review` mode, open its review. `true` when the mode is one of them.
    pub(in crate::root) fn open_ui_fixture_broadcaster_progress(
        &mut self,
        asset: &crate::root::UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        use TransactionGenerationStage::WaitingForBroadcasterResponse;
        use wallet_ops::SubmissionRouteState::{Ready, Reconnecting};
        if holds_send_review() {
            self.open_ui_fixture_broadcaster_review(asset, window, cx);
            return true;
        }
        let Some(Mode::BroadcasterProgress(state)) = mode() else {
            return false;
        };
        // The recipient is the wallet's own address, so the dialog shows no stranger's.
        let recipient = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
            .unwrap_or_default();
        let key = crate::root::UnshieldAssetKey::from_asset(asset);
        if state == BroadcasterProgress::Refused {
            // A Send form backs the refusal's actions; it clears progress, so it comes first.
            self.initialize_send_form(asset.clone(), window, cx);
            if let Some(form) = self.send_forms.get_mut(&key) {
                form.generation_id = u64::MAX;
            }
        }
        let route = self.start_private_broadcaster_progress(
            crate::root::DeliveryFormKind::Send,
            key,
            u64::MAX,
            asset.label.clone(),
            asset.icon_path.clone(),
            recipient,
            None,
            Duration::from_mins(2),
            cx,
        );
        route.send_replace(match state {
            BroadcasterProgress::Ready
            | BroadcasterProgress::Refused
            | BroadcasterProgress::Direct => SubmissionRouteProgress {
                state: Ready,
                peers: 3,
                recoveries_used: 0,
                publishes: 4,
            },
            BroadcasterProgress::Reconnecting => SubmissionRouteProgress {
                state: Reconnecting,
                peers: 0,
                recoveries_used: 1,
                publishes: 3,
            },
            // The route was lost again after its last reconnect, and no answer came.
            BroadcasterProgress::Timeout => SubmissionRouteProgress {
                state: Reconnecting,
                peers: 0,
                recoveries_used: SUBMISSION_ROUTE_RECOVERY_LIMIT,
                publishes: 9,
            },
        });
        hold_route(route);
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            apply_private_broadcaster_progress_stage(
                &mut progress.steps,
                WaitingForBroadcasterResponse,
            );
            progress.stage_seen = true;
            progress.stop_available = false;
            progress.public_broadcaster_wait_started_at =
                std::time::Instant::now().checked_sub(Duration::from_secs(17));
            progress.route_isolated = state != BroadcasterProgress::Direct;
            if state == BroadcasterProgress::Timeout {
                finish_private_broadcaster_progress_steps_at_stage(
                    &mut progress.steps,
                    WaitingForBroadcasterResponse,
                    &PublicBroadcasterResultKind::TimedOut,
                );
            }
            if state == BroadcasterProgress::Refused {
                let entered_amount = U256::from(10).pow(U256::from(asset.decimals.unwrap_or(18)));
                let fee_amount = entered_amount / U256::from(100);
                let result = wallet_ops::PublicBroadcasterSubmissionResult {
                    broadcaster: wallet_ops::PublicBroadcasterCandidate {
                        token: asset.token,
                        fee: U256::from(1_000_000_000_u64),
                        fee_expiration: std::time::SystemTime::now() + Duration::from_mins(1),
                        ..crate::root::private_swap::setup_progress::stand_in_setup_broadcaster(
                            asset.chain_id,
                            None,
                        )
                    },
                    action_token: asset.token,
                    fee_token: asset.token,
                    entered_amount,
                    receiver_amount: entered_amount - fee_amount,
                    recipient_amount: entered_amount - fee_amount,
                    total_private_spend: entered_amount,
                    fee_amount,
                    protocol_fee_amount: U256::ZERO,
                    protocol_fee_bps: U256::ZERO,
                    fee_mode: wallet_ops::FeeHandlingMode::DeductFromAmount,
                    gas_limit: 1_175_614,
                    min_gas_price: 58_798_000,
                    transaction_count: 1,
                    input_count: 1,
                    private_output_count: 1,
                    public_output_count: 0,
                    relay_call_count: 0,
                    uses_relay_adapt: false,
                    result: PublicBroadcasterResultKind::Failed {
                        error: "Gas cost is too high, please refresh and try again".to_owned(),
                    },
                    native_top_up: None,
                    payload_inputs: None,
                };
                finish_private_broadcaster_progress_steps_at_stage(
                    &mut progress.steps,
                    WaitingForBroadcasterResponse,
                    &result.result,
                );
                progress.result = Some(result);
            }
        }
        self.show_private_broadcaster_progress_dialog(window, cx);
        true
    }

    /// Fill a Send form of `asset` with 0.5 to the wallet's own address and a stand-in quote of
    /// two plain transactions whose fee is paid in USDC, then submit it, which opens the real
    /// review. No estimate runs, so nothing replaces the quote.
    fn open_ui_fixture_broadcaster_review(
        &mut self,
        asset: &crate::root::UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let recipient = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
            .unwrap_or_default();
        let fee_token = [asset.chain_id, 1]
            .into_iter()
            .find_map(|chain| known_tokens_for_chain(chain).find(|token| token.symbol == "USDC"))
            .map_or(Address::ZERO, |token| token.token);
        let entered_amount =
            U256::from(10).pow(U256::from(asset.decimals.unwrap_or(18))) / U256::from(2);
        let gas_limit = 2_351_228;
        let min_gas_price = 58_798_000;
        let estimate = wallet_ops::PublicBroadcasterCostEstimate {
            broadcaster: wallet_ops::PublicBroadcasterCandidate {
                token: fee_token,
                fee: U256::from(1_000_000_000_u64),
                fee_expiration: std::time::SystemTime::now() + Duration::from_mins(1),
                ..crate::root::private_swap::setup_progress::stand_in_setup_broadcaster(
                    asset.chain_id,
                    None,
                )
            },
            action_token: asset.token,
            fee_token,
            entered_amount,
            receiver_amount: entered_amount,
            recipient_amount: entered_amount,
            total_private_spend: entered_amount,
            // 1.25 USDC.
            fee_amount: U256::from(1_250_000_u64),
            protocol_fee_amount: U256::ZERO,
            protocol_fee_bps: U256::ZERO,
            fee_mode: wallet_ops::FeeHandlingMode::AddToAmount,
            max_receiver_amount: asset.max_batched,
            max_entered_amount: asset.max_batched,
            gas_limit,
            min_gas_price,
            native_gas_cost: U256::from(gas_limit) * U256::from(min_gas_price),
            transaction_count: 2,
            input_count: 2,
            private_output_count: 2,
            public_output_count: 0,
            relay_call_count: 0,
            uses_relay_adapt: false,
            native_top_up: None,
        };
        let key = crate::root::UnshieldAssetKey::from_asset(asset);
        self.initialize_send_form(asset.clone(), window, cx);
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        // `set_value` emits no change, so no estimate is scheduled to replace the quote.
        form.recipient_input.update(cx, |input, cx| {
            input.set_value(recipient.clone(), window, cx);
        });
        form.amount_input
            .update(cx, |input, cx| input.set_value("0.5", window, cx));
        form.recipient_value = recipient.into();
        form.selected_fee_token = fee_token;
        form.cost_estimate = Some(estimate);
        self.generate_send_calldata_from_form(key, window, cx);
    }
}
