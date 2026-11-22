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
        let (fee_token, estimate) = stand_in_cost_estimate(asset);
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

    /// In a `hints:` mode, fill the Unshield form of `key`, just opened, with 0.5 to the first
    /// Public account's address, or to 0x1111…1111 without one, so Submit opens its review over
    /// the stand-in estimate.
    pub(in crate::root) fn stage_ui_fixture_hints_unshield_inputs(
        &mut self,
        key: crate::root::UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        const STAND_IN_RECIPIENT: Address = Address::repeat_byte(0x11);
        if !hints_active() {
            return;
        }
        let recipient = self
            .public_accounts
            .first()
            .map_or(STAND_IN_RECIPIENT, |account| account.address)
            .to_checksum(None);
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        // `set_value` emits no change, so nothing drops the stand-in estimate.
        form.recipient_input.update(cx, |input, cx| {
            input.set_value(recipient.clone(), window, cx);
        });
        form.amount_input
            .update(cx, |input, cx| input.set_value("0.5", window, cx));
        form.recipient_value = recipient.into();
    }

    /// In a `delivery:` mode, set the `kind` form of `key`, just opened, to the mode's state. A
    /// `hints:` mode holds the `estimate` state, whose Unshield estimate carries the mode's plan.
    /// No scheduled form estimate runs in the mode to replace it.
    pub(in crate::root) fn apply_ui_fixture_delivery(
        &mut self,
        kind: crate::root::DeliveryFormKind,
        key: crate::root::UnshieldAssetKey,
    ) {
        use crate::root::private_action::{SelfBroadcastFundingMode, SendResult, UnshieldResult};
        use crate::root::{DeliveryFormKind, DeliveryMode};
        let state = match mode() {
            Some(Mode::Delivery(state)) => state,
            Some(Mode::Hints(_)) => Delivery::Estimate,
            _ => return,
        };
        let Some(asset) = self.private_action_dialog_asset(kind, key).cloned() else {
            return;
        };
        let (fee_token, estimate) = stand_in_form_cost_estimate(kind, &asset);
        let fee_mode = estimate.fee_mode;
        // The custom fee is the stand-in fee, 1.25 of its token.
        let custom_fee = (state == Delivery::CustomFee).then_some(estimate.fee_amount);
        let result = (state == Delivery::Submitted).then(|| stand_in_submission(&estimate));
        let estimate = matches!(
            state,
            Delivery::Estimate | Delivery::Refreshing | Delivery::Submitted
        )
        .then_some(estimate);
        let sponsored = state == Delivery::Sponsored;
        let wrapped_native = self
            .effective_chain_configs
            .get(key.chain_id)
            .and_then(|chain| chain.wrapped_native_token);
        let sponsored_estimate =
            sponsored.then(|| stand_in_sponsored_estimate(key.chain_id, wrapped_native));
        let failed = matches!(state, Delivery::Retry | Delivery::PinConflict);
        let pin_conflict = state == Delivery::PinConflict;
        // The two forms name these fields alike; only their result types differ.
        macro_rules! hold {
            ($form:ident, $result:path) => {{
                if sponsored {
                    $form.delivery_mode = DeliveryMode::SelfBroadcast;
                    $form.self_broadcast_funding = SelfBroadcastFundingMode::PrivateSponsorship;
                    $form.sponsored_funding_estimate = sponsored_estimate;
                } else {
                    $form.delivery_mode = DeliveryMode::PublicBroadcaster;
                    $form.selected_fee_token = fee_token;
                    $form.fee_mode = fee_mode;
                }
                $form.cost_estimate = estimate;
                $form.estimating_cost = state == Delivery::Refreshing;
                $form.custom_fee_amount = custom_fee;
                $form.error = failed.then(|| "The UI fixture's stand-in estimate failed.".into());
                $form.input_pin = pin_conflict.then(stand_in_input_pin);
                $form.input_pin_conflict = pin_conflict;
                $form.result = result.map($result);
            }};
        }
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key) {
                    hold!(form, SendResult::PublicBroadcaster);
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key) {
                    // Keep unwrap off in held Unshield forms regardless of the saved default.
                    form.unwrap = false;
                    hold!(form, UnshieldResult::PublicBroadcaster);
                }
            }
        }
    }

    /// In a `delivery:` mode, drop the stand-in estimate of the `kind` form of `key` unless
    /// `retain_shown`. The fixture refuses every estimate, so an edit that would restart one
    /// drops the stand-in quote instead.
    pub(in crate::root) fn drop_ui_fixture_delivery_estimate(
        &mut self,
        kind: crate::root::DeliveryFormKind,
        key: crate::root::UnshieldAssetKey,
        retain_shown: bool,
        cx: &mut Context<'_, Self>,
    ) {
        use crate::root::DeliveryFormKind;
        if retain_shown || !holds_delivery() {
            return;
        }
        let estimate = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get_mut(&key)
                .map(|form| &mut form.cost_estimate),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get_mut(&key)
                .map(|form| &mut form.cost_estimate),
        };
        if let Some(estimate) = estimate
            && estimate.take().is_some()
        {
            cx.notify();
        }
    }

    /// The fee tokens Send and Unshield offer on `chain_id` in a `delivery:estimate`,
    /// `delivery:pin-conflict` or `hints:` mode: only the stand-in estimate's, 1000 of it
    /// spendable, with one eligible broadcaster, so the form's Submit is enabled. A paired Gateway
    /// client's private broadcaster drafts get the same option in place of the wallet's own fee
    /// tokens. `None` in every other mode.
    pub(in crate::root) fn ui_fixture_fee_token_options(
        chain_id: u64,
    ) -> Option<Vec<crate::root::public_broadcaster::PublicBroadcasterFeeTokenOption>> {
        if !matches!(
            mode(),
            Some(Mode::Delivery(Delivery::Estimate | Delivery::PinConflict) | Mode::Hints(_))
        ) {
            return None;
        }
        let fee_token = stand_in_fee_token(chain_id);
        Some(vec![
            crate::root::public_broadcaster::PublicBroadcasterFeeTokenOption {
                token: fee_token.token,
                label: fee_token.symbol.to_owned(),
                decimals: Some(fee_token.decimals),
                max_spendable: U256::from(1000)
                    * U256::from(10).pow(U256::from(fee_token.decimals)),
                eligible_broadcaster_count: 1,
                icon_path: token_icon_asset_path(fee_token.chain_id, &fee_token.token)
                    .map(WalletIconSource::embedded),
            },
        ])
    }
}

/// `estimate` accepted by its broadcaster, as a `delivery:submitted` form holds it.
fn stand_in_submission(
    estimate: &wallet_ops::PublicBroadcasterCostEstimate,
) -> Box<wallet_ops::PublicBroadcasterSubmissionResult> {
    Box::new(wallet_ops::PublicBroadcasterSubmissionResult {
        broadcaster: estimate.broadcaster.clone(),
        action_token: estimate.action_token,
        fee_token: estimate.fee_token,
        entered_amount: estimate.entered_amount,
        receiver_amount: estimate.receiver_amount,
        recipient_amount: estimate.recipient_amount,
        total_private_spend: estimate.total_private_spend,
        fee_amount: estimate.fee_amount,
        protocol_fee_amount: estimate.protocol_fee_amount,
        protocol_fee_bps: estimate.protocol_fee_bps,
        fee_mode: estimate.fee_mode,
        gas_limit: estimate.gas_limit,
        min_gas_price: estimate.min_gas_price,
        transaction_count: estimate.transaction_count,
        input_count: estimate.input_count,
        private_output_count: estimate.private_output_count,
        public_output_count: estimate.public_output_count,
        relay_call_count: estimate.relay_call_count,
        uses_relay_adapt: estimate.uses_relay_adapt,
        result: PublicBroadcasterResultKind::Submitted {
            tx_hash: alloy::hex::encode_prefixed(B256::repeat_byte(0x5d)),
        },
        native_top_up: None,
        payload_inputs: None,
    })
}

/// The sponsored estimate of a `delivery:sponsored` form on `chain_id`: too little private
/// `wrapped_native` for its fees, 0 available and 0.01 required.
fn stand_in_sponsored_estimate(
    chain_id: u64,
    wrapped_native: Option<Address>,
) -> crate::root::private_action::SponsoredFundingEstimateState {
    crate::root::private_action::SponsoredFundingEstimateState::InsufficientWrappedNative {
        chain_id,
        token: wrapped_native,
        available: U256::ZERO,
        required: U256::from(10_000_000_000_000_000_u64),
    }
}

/// The pinned notes of a `delivery:pin-conflict` form.
fn stand_in_input_pin() -> wallet_ops::PublicBroadcasterInputPin {
    wallet_ops::PublicBroadcasterInputPin {
        inputs: vec![wallet_ops::SelectedInputIdentity {
            tree: 0,
            position: 7,
        }],
        overridden: false,
    }
}

/// The fee token of the stand-in estimate on `chain_id`: its network's USDC, or Ethereum's
/// where it lists none. A zero-address USDC of 6 decimals where neither is known. A `hints:`
/// mode shields it too.
pub(super) fn stand_in_fee_token(chain_id: u64) -> KnownTokenInfo {
    [chain_id, 1]
        .into_iter()
        .find_map(|chain| known_tokens_for_chain(chain).find(|token| token.symbol == "USDC"))
        .unwrap_or(KnownTokenInfo {
            chain_id,
            token: Address::ZERO,
            symbol: "USDC",
            decimals: 6,
            anchor_sources: &[],
        })
}

/// The stand-in broadcaster estimate of a Send or Unshield of 0.5 of `asset`: two plain
/// transactions whose fee is 1.25 of the USDC of `asset`'s network, or of Ethereum's where it
/// lists none, which then shows in raw units. Both forms use this Send-shaped estimate, with no
/// public output and no Unshield protocol fee. Also returns that fee token.
fn stand_in_cost_estimate(
    asset: &crate::root::UnshieldAsset,
) -> (Address, wallet_ops::PublicBroadcasterCostEstimate) {
    let fee = stand_in_fee_token(asset.chain_id);
    let fee_token = fee.token;
    let entered_amount =
        U256::from(10).pow(U256::from(asset.decimals.unwrap_or(18))) / U256::from(2);
    // 1.25 of the fee token.
    let fee_amount =
        U256::from(125) * U256::from(10).pow(U256::from(fee.decimals)) / U256::from(100);
    // With AddToAmount, a fee in the payment token increases its total private spend.
    let total_private_spend = if asset.token == fee_token {
        entered_amount + fee_amount
    } else {
        entered_amount
    };
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
        total_private_spend,
        fee_amount,
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
        planned_unshields: None,
    };
    (fee_token, estimate)
}

/// The stand-in estimate of the `kind` form, whose Unshield estimate carries a `hints:` mode's
/// plan and, in that mode, deducts fees from the amount. Also returns its fee token.
fn stand_in_form_cost_estimate(
    kind: crate::root::DeliveryFormKind,
    asset: &crate::root::UnshieldAsset,
) -> (Address, wallet_ops::PublicBroadcasterCostEstimate) {
    let (fee_token, mut estimate) = stand_in_cost_estimate(asset);
    if matches!(kind, crate::root::DeliveryFormKind::Unshield) {
        estimate.planned_unshields = super::amount_hints::stand_in_planned_unshields(asset);
        // The form copies this mode, in which its common exit amounts can be chosen.
        if hints_active() {
            estimate.fee_mode = wallet_ops::FeeHandlingMode::DeductFromAmount;
        }
    }
    (fee_token, estimate)
}
