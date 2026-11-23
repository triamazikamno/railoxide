use std::rc::Rc;

use super::generation::{
    broadcaster_estimate_allows_submission, broadcaster_input_pin_conflict_awaits_choice,
};
use super::{
    Address, Alert, App, BroadcasterChoice, Button, ButtonVariants, DeliveryFormKind, DeliveryMode,
    Disableable, Eip1559GasFeeEditorState, Entity, FeeHandlingMode, FullWidthSelectItems,
    InputState, InteractiveElement, ParentElement, Pixels, PublicBroadcasterCostEstimate,
    PublicBroadcasterFeeTokenOption, RecipientOption, ScrollHandle, SelectState,
    SelfBroadcastFundingMode, SelfBroadcastGasPayerSelectItem, SendFormState, SendResult,
    SharedString, Sizable, SponsoredFundingEstimateState, SponsoredIncentive,
    SponsoredSelfBroadcastSessionOutcome, Styled, TransactionGenerationStage, U256, UnshieldAsset,
    UnshieldAssetKey, UnshieldFormState, UnshieldResult, WalletRoot, Window, app_button,
    app_muted_text, app_strong_text, delivery_element_id, div,
    effective_self_broadcast_funding_mode, fee_policy_eligible_public_broadcasters,
    format_form_error_for_asset, is_effective_wrapped_native_token, labeled_field,
    private_action_amount_input, private_action_metric_display_amount,
    private_broadcaster_closed_active_progress, public_broadcaster_cost_status,
    public_broadcaster_fee_token_warning, public_broadcaster_submit_disabled_for_fee_token_options,
    px, render_delivery_selector, render_fee_mode_toggle, render_private_action_amount_field,
    render_private_broadcaster_status_notice, render_private_self_broadcast_status_notice,
    render_private_submission_active_status_notice, render_public_broadcaster_cost_estimate,
    render_public_broadcaster_cost_status, render_public_broadcaster_settings,
    render_recipient_picker, render_self_broadcast_settings, render_send_result,
    render_sponsored_funding_estimate, render_unshield_generating_status,
    render_unshield_native_top_up_control, render_unshield_output_toggle, render_unshield_result,
    selected_broadcaster_fee_warning, send_element_id, send_form_max_label, send_form_submitted,
    should_render_public_broadcaster_cost_preview, sponsored_estimate_allows_submission,
    sponsored_funding_choice_visible, sponsored_funding_enabled,
    sponsored_self_broadcast_availability_reason, unshield_element_id, unshield_form_max_label,
    unshield_form_submitted,
};
use crate::root::amount_privacy::{UseAmount, render_amount_privacy_hints};

/// One Send or Unshield form, as the shared form sections read it.
struct DeliveryFormView<'a> {
    root: &'a Entity<WalletRoot>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    asset: &'a UnshieldAsset,
    generation_ready: bool,
    submitted: bool,
    has_result: bool,
    error: Option<&'a str>,
    delivery_mode: DeliveryMode,
    fee_mode: FeeHandlingMode,
    generating: bool,
    gateway_executing: bool,
    generation_id: u64,
    generation_stage: TransactionGenerationStage,
    /// Public broadcaster lookup flags; always off for Send.
    unwrap: bool,
    native_top_up: bool,
    allow_suspicious_broadcasters: bool,
    favorites_only_broadcasters: bool,
    broadcaster_choice: &'a BroadcasterChoice,
    selected_fee_token: Address,
    cost_estimate: Option<&'a PublicBroadcasterCostEstimate>,
    cost_estimate_pending: bool,
    estimating_cost: bool,
    custom_fee_amount: Option<U256>,
    transaction_fee_breakdown_open: bool,
    pin_conflict_awaits_choice: bool,
    self_broadcast_gas_payer_uuid: Option<&'a str>,
    self_broadcast_gas_payer_select:
        &'a Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>>,
    self_broadcast_gas_fee: &'a Eip1559GasFeeEditorState,
    self_broadcast_funding: SelfBroadcastFundingMode,
    sponsored_incentive: SponsoredIncentive,
    sponsored_custom_incentive_input: &'a Entity<InputState>,
    sponsored_funding_estimate: Option<&'a SponsoredFundingEstimateState>,
    sponsored_estimate_pending: bool,
    sponsored_funding_breakdown_open: bool,
    recipient_input: &'a Entity<InputState>,
    recipient_value: &'a str,
    recipient_suggestions_open: bool,
    recipient_suggestion_index: Option<usize>,
    recipient_suggestions_scroll: &'a ScrollHandle,
    amount_input: &'a Entity<InputState>,
}

/// Builds a `DeliveryFormView` from either form state, which name these fields alike. The
/// trailing `field: value` pairs are the per-form rest.
macro_rules! delivery_form_view {
    ($wallet:expr, $root:expr, $key:expr, $form:ident, $($field:ident: $value:expr),+ $(,)?) => {
        DeliveryFormView {
            $($field: $value,)+
            root: $root,
            key: $key,
            asset: &$form.asset,
            generation_ready: $wallet.private_action_generation_ready($form.asset.chain_id),
            has_result: $form.result.is_some(),
            error: $form.error.as_deref(),
            delivery_mode: $form.delivery_mode,
            fee_mode: $form.fee_mode,
            generating: $form.generating,
            gateway_executing: $form.gateway_execution.is_some(),
            generation_id: $form.generation_id,
            generation_stage: $form.generation_stage,
            allow_suspicious_broadcasters: $form.allow_suspicious_broadcasters,
            favorites_only_broadcasters: $form.favorites_only_broadcasters,
            broadcaster_choice: &$form.broadcaster_choice,
            selected_fee_token: $form.selected_fee_token,
            cost_estimate: $form.cost_estimate.as_ref(),
            cost_estimate_pending: $form.cost_estimate_pending,
            estimating_cost: $form.estimating_cost,
            custom_fee_amount: $form.custom_fee_amount,
            transaction_fee_breakdown_open: $form.transaction_fee_breakdown_open,
            pin_conflict_awaits_choice: broadcaster_input_pin_conflict_awaits_choice(
                $form.input_pin.as_ref(),
                $form.input_pin_conflict,
                $form.cost_estimate_pending || $form.estimating_cost,
            ),
            self_broadcast_gas_payer_uuid: $form.self_broadcast_gas_payer_uuid.as_deref(),
            self_broadcast_gas_payer_select: &$form.self_broadcast_gas_payer_select,
            self_broadcast_gas_fee: &$form.self_broadcast_gas_fee,
            self_broadcast_funding: $form.self_broadcast_funding,
            sponsored_incentive: $form.sponsored_incentive,
            sponsored_custom_incentive_input: &$form.sponsored_custom_incentive_input,
            sponsored_funding_estimate: $form.sponsored_funding_estimate.as_ref(),
            sponsored_estimate_pending: $form.sponsored_estimate_pending,
            sponsored_funding_breakdown_open: $form.sponsored_funding_breakdown_open,
            recipient_input: &$form.recipient_input,
            recipient_value: &$form.recipient_value,
            recipient_suggestions_open: $form.recipient_suggestions_open,
            recipient_suggestion_index: $form.recipient_suggestion_index,
            recipient_suggestions_scroll: &$form.recipient_suggestions_scroll,
            amount_input: &$form.amount_input,
        }
    };
}

impl WalletRoot {
    pub(in crate::root) fn render_send_form(
        &self,
        root: Entity<Self>,
        key: UnshieldAssetKey,
        content_width: Pixels,
    ) -> gpui::Div {
        let Some(form) = self.send_forms.get(&key) else {
            return div();
        };
        let view = delivery_form_view!(self, &root, key, form,
            kind: DeliveryFormKind::Send,
            submitted: send_form_submitted(form),
            unwrap: false,
            native_top_up: false,
        );
        let mut card = div().w(content_width).flex().flex_col().gap_3();

        if form.asset.total > form.asset.max_batched {
            card = card.child(
                ui::private_action::warning(
                    send_element_id(key, "spend-capacity-warning"),
                    ui::private_action::spend_capacity_warning(false),
                )
                .small(),
            );
        }

        let (card, submit_disabled) = self.render_send_inputs(&view, form, card);
        let card = self.render_submit_section(&view, card, submit_disabled);
        let mut card = self.render_delivery_status(&view, card);

        if let Some(result) = form.result.as_ref() {
            card = card.child(match result {
                SendResult::Manual(result) => render_send_result(key, result),
                SendResult::PublicBroadcaster(result) => render_private_broadcaster_status_notice(
                    root,
                    key,
                    DeliveryFormKind::Send,
                    &result.result,
                ),
                SendResult::SelfBroadcast(result) => div().flex().flex_col().gap_2().child(
                    render_private_self_broadcast_status_notice(
                        root,
                        key,
                        DeliveryFormKind::Send,
                        result,
                    ),
                ),
                SendResult::Sponsored(result) => render_sponsored_self_broadcast_result(result),
            });
        }

        card
    }

    pub(in crate::root) fn render_unshield_form(
        &self,
        root: Entity<Self>,
        key: UnshieldAssetKey,
        content_width: Pixels,
        cx: &App,
    ) -> gpui::Div {
        let Some(form) = self.unshield_forms.get(&key) else {
            return div();
        };
        let view = delivery_form_view!(self, &root, key, form,
            kind: DeliveryFormKind::Unshield,
            submitted: unshield_form_submitted(form),
            unwrap: form.unwrap,
            native_top_up: form.native_top_up_enabled && form.native_top_up.is_some(),
        );
        let mut card = div().w(content_width).flex().flex_col().gap_3();

        if form.native_top_up_enabled {
            card = card.child(ui::private_action::warning(
                unshield_element_id(key, "top-up-linkage"),
                ui::private_action::NATIVE_TOP_UP_LINKAGE_WARNING,
            ));
        }

        if form.asset.total > form.asset.max_batched {
            card = card.child(
                ui::private_action::warning(
                    unshield_element_id(key, "spend-capacity-warning"),
                    ui::private_action::spend_capacity_warning(true),
                )
                .small(),
            );
        }

        let (card, submit_disabled) = self.render_unshield_inputs(&view, form, card, cx);
        let card = self.render_submit_section(&view, card, submit_disabled);
        let mut card = self.render_delivery_status(&view, card);

        if let Some(result) = form.result.as_ref() {
            card = card.child(match result {
                UnshieldResult::Manual(result) => render_unshield_result(key, &form.asset, result),
                UnshieldResult::PublicBroadcaster(result) => {
                    render_private_broadcaster_status_notice(
                        root,
                        key,
                        DeliveryFormKind::Unshield,
                        &result.result,
                    )
                }
                UnshieldResult::SelfBroadcast(result) => div().flex().flex_col().gap_2().child(
                    render_private_self_broadcast_status_notice(
                        root,
                        key,
                        DeliveryFormKind::Unshield,
                        result,
                    ),
                ),
                UnshieldResult::Sponsored(result) => render_sponsored_self_broadcast_result(result),
            });
        }

        card
    }

    /// Delivery controls, then Send's recipient and amount row. Also returns whether submit is
    /// disabled.
    fn render_send_inputs(
        &self,
        view: &DeliveryFormView<'_>,
        form: &SendFormState,
        card: gpui::Div,
    ) -> (gpui::Div, bool) {
        let (card, submit_disabled) = self.render_delivery_controls(view, card);
        let card = card.child(
            div()
                .flex()
                .items_end()
                .gap_3()
                .child(view.recipient_field(
                    "Recipient 0zk address",
                    &self.private_send_recipient_options(),
                ))
                .child(view.amount_field(send_form_max_label(form), submit_disabled)),
        );
        (card, submit_disabled)
    }

    /// Delivery controls, then Unshield's fee-mode toggle, recipient, output and amount row,
    /// privacy hints, gas top-up and executor reason. Also returns whether submit is disabled.
    fn render_unshield_inputs(
        &self,
        view: &DeliveryFormView<'_>,
        form: &UnshieldFormState,
        card: gpui::Div,
        cx: &App,
    ) -> (gpui::Div, bool) {
        let unwrap_supported = is_effective_wrapped_native_token(
            &self.effective_chain_configs,
            form.asset.chain_id,
            form.asset.token,
        );
        // Without an executor route, unwrap and top-up can't be turned on. One already on stays
        // switchable off.
        let executor_unavailable = self.unshield_form_executor_unavailable(form);
        let executor_reason = executor_unavailable
            .filter(|_| unwrap_supported || form.native_top_up.is_some())
            .map(|reason| app_muted_text(reason.to_string()).whitespace_normal());
        let (card, submit_disabled) = self.render_delivery_controls(view, card);
        let card = card
            .child(render_fee_mode_toggle(
                view.root.clone(),
                view.key,
                DeliveryFormKind::Unshield,
                form.delivery_mode,
                form.fee_mode,
                form.generating,
            ))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_3()
                    .child(
                        view.recipient_field(
                            "Recipient",
                            &self.private_unshield_recipient_options(),
                        ),
                    )
                    .children(unwrap_supported.then(|| {
                        render_unshield_output_toggle(
                            view.root.clone(),
                            view.key,
                            form.asset.chain_id,
                            form.unwrap,
                            form.generating || (executor_unavailable.is_some() && !form.unwrap),
                        )
                    }))
                    .child(view.amount_field(unshield_form_max_label(form), submit_disabled)),
            )
            .children(self.render_unshield_privacy_hints(view, cx))
            .child(render_unshield_native_top_up_control(
                view.root.clone(),
                view.key,
                form.native_top_up.as_ref(),
                form.native_top_up_enabled,
                form.generating || (executor_unavailable.is_some() && !form.native_top_up_enabled),
                Some(&self.effective_token_registry),
            ))
            .children(executor_reason);
        (card, submit_disabled)
    }

    /// The Unshield form's privacy hints, `None` without a form for its key, whose common exit
    /// amounts are offered until generation starts. With fees added on top the list is
    /// browse-only.
    fn render_unshield_privacy_hints(
        &self,
        view: &DeliveryFormView<'_>,
        cx: &App,
    ) -> Option<gpui::Div> {
        let key = view.key;
        self.unshield_privacy_presentation(key).map(|presentation| {
            let root = view.root.clone();
            let use_amount: UseAmount = Rc::new(move |amount, window, cx| {
                root.update(cx, |root, cx| {
                    root.use_unshield_amount(key, amount, window, cx);
                });
            });
            render_amount_privacy_hints(
                &unshield_element_id(key, "privacy"),
                &presentation,
                (!view.generating).then_some(use_amount),
                cx,
            )
        })
    }

    /// Delivery selector, then the selected mode's settings. Also returns whether submit is
    /// disabled.
    fn render_delivery_controls(
        &self,
        view: &DeliveryFormView<'_>,
        card: gpui::Div,
    ) -> (gpui::Div, bool) {
        let self_broadcast_accounts = self.active_self_broadcast_gas_payer_accounts();
        let effective_chain = self.effective_chain_configs.get(view.asset.chain_id);
        let show_sponsored_funding_choice = sponsored_funding_choice_visible(effective_chain);
        let sponsorship_unavailable_reason = show_sponsored_funding_choice
            .then(|| sponsored_self_broadcast_availability_reason(effective_chain))
            .flatten();
        let sponsorship_enabled = sponsored_funding_enabled(
            show_sponsored_funding_choice,
            sponsorship_unavailable_reason,
        );
        let card = card.child(render_delivery_selector(
            view.root.clone(),
            view.key,
            view.kind,
            view.delivery_mode,
            view.generating || view.gateway_executing,
            !self_broadcast_accounts.is_empty(),
            sponsorship_enabled,
        ));
        let base_submit_disabled = !view.generation_ready || view.generating || view.submitted;
        match view.delivery_mode {
            DeliveryMode::PublicBroadcaster => {
                let (card, broadcaster_disabled) =
                    self.render_public_broadcaster_controls(view, card);
                (card, base_submit_disabled || broadcaster_disabled)
            }
            DeliveryMode::SelfBroadcast => {
                let submit_disabled = base_submit_disabled
                    || self.self_broadcast_submit_disabled(view, effective_chain);
                let card = card.child(render_self_broadcast_settings(
                    view.root.clone(),
                    view.key,
                    view.kind,
                    &self_broadcast_accounts,
                    view.self_broadcast_gas_payer_uuid,
                    self.public_balance_snapshot.as_deref(),
                    view.self_broadcast_gas_payer_select,
                    view.self_broadcast_gas_fee,
                    view.self_broadcast_funding,
                    view.sponsored_incentive,
                    view.sponsored_custom_incentive_input,
                    show_sponsored_funding_choice,
                    sponsorship_unavailable_reason,
                    view.generating,
                    !submit_disabled,
                    view.generate_handler(),
                ));
                (card, submit_disabled)
            }
            DeliveryMode::ManualCalldata => (card, base_submit_disabled),
        }
    }

    /// Fee-token warning, broadcaster settings and fee-policy warning. Also returns whether the
    /// broadcaster state disables submit.
    fn render_public_broadcaster_controls(
        &self,
        view: &DeliveryFormView<'_>,
        card: gpui::Div,
    ) -> (gpui::Div, bool) {
        let policy = self.public_broadcaster_fee_policy(view.allow_suspicious_broadcasters);
        let trust_filter = self.public_broadcaster_trust_filter(view.favorites_only_broadcasters);
        let fee_token_options = self.current_public_broadcaster_fee_token_options(
            view.asset.chain_id,
            view.unwrap,
            view.native_top_up,
            view.favorites_only_broadcasters,
            policy,
        );
        let candidates = self.current_public_broadcaster_candidates(
            view.asset.chain_id,
            view.selected_fee_token,
            view.unwrap,
            view.native_top_up,
            view.favorites_only_broadcasters,
            policy,
        );
        let visible_candidates = fee_policy_eligible_public_broadcasters(&candidates, policy);
        let fee_token_warning = public_broadcaster_fee_token_warning(
            &self.monitor_fee_rows(),
            view.asset.chain_id,
            &fee_token_options,
            view.selected_fee_token,
            &trust_filter,
        );
        let fee_policy_warning = selected_broadcaster_fee_warning(
            view.broadcaster_choice,
            &candidates,
            view.allow_suspicious_broadcasters,
        );
        let card = card
            .children(fee_token_warning.map(|warning| {
                ui::private_action::warning(view.element_id("fee-token-warning"), warning).small()
            }))
            .child(render_public_broadcaster_settings(
                view.root.clone(),
                view.key,
                view.kind,
                view.allow_suspicious_broadcasters,
                view.favorites_only_broadcasters,
                view.asset.token,
                view.fee_mode,
                view.broadcaster_choice,
                &visible_candidates,
                &fee_token_options,
                view.selected_fee_token,
                view.generating,
            ))
            .children(fee_policy_warning.map(|warning| {
                ui::private_action::warning(view.element_id("fee-policy-warning"), warning).small()
            }));
        (
            card,
            view.public_broadcaster_submit_disabled(&fee_token_options),
        )
    }

    /// Self-broadcast needs an available gas payer and, when sponsored, a ready estimate.
    fn self_broadcast_submit_disabled(
        &self,
        view: &DeliveryFormView<'_>,
        effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    ) -> bool {
        let effective_funding =
            effective_self_broadcast_funding_mode(effective_chain, view.self_broadcast_funding);
        let sponsored_estimate_ready = sponsored_estimate_allows_submission(
            effective_funding,
            view.sponsored_funding_estimate,
            view.sponsored_estimate_pending,
        );
        view.self_broadcast_gas_payer_uuid
            .and_then(|uuid| self.selected_self_broadcast_gas_payer_account(Some(uuid)))
            .is_none()
            || !sponsored_estimate_ready
    }

    /// Sponsored funding estimate, the submit button and the custom-fee row.
    fn render_submit_section(
        &self,
        view: &DeliveryFormView<'_>,
        mut card: gpui::Div,
        submit_disabled: bool,
    ) -> gpui::Div {
        if view.delivery_mode == DeliveryMode::SelfBroadcast
            && let Some(estimate_state) = view.sponsored_funding_estimate
        {
            card = card.child(render_sponsored_funding_estimate(
                view.root.clone(),
                view.key,
                view.kind,
                &self.sponsored_funding_estimate_display(estimate_state),
                view.sponsored_funding_breakdown_open,
            ));
        }
        card = card.child(
            div()
                .flex()
                .items_end()
                .gap_3()
                .justify_end()
                .child(view.submit_button(submit_disabled)),
        );
        if view.custom_fee_row_visible() {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(app_muted_text("Custom transaction fee"))
                    .child(super::fee_editor::fee_edit_button(
                        view.root.clone(),
                        view.kind,
                        view.key,
                    )),
            );
        }
        card
    }

    /// Cost preview or status, submission progress and the form error.
    fn render_delivery_status(
        &self,
        view: &DeliveryFormView<'_>,
        mut card: gpui::Div,
    ) -> gpui::Div {
        if should_render_public_broadcaster_cost_preview(
            view.delivery_mode,
            view.has_result,
            view.error.is_some(),
        ) {
            card = card.children(self.render_cost_preview(view));
        }

        if view.broadcast_generating()
            && let Some(active) = private_broadcaster_closed_active_progress(
                self.private_broadcaster_progress.as_ref(),
                view.kind,
                view.key,
                view.generation_id,
            )
        {
            card = card.child(render_private_submission_active_status_notice(
                view.root.clone(),
                view.key,
                view.kind,
                &active,
            ));
        }

        if view.generating && view.delivery_mode == DeliveryMode::ManualCalldata {
            card = card.child(render_unshield_generating_status(
                self.unshield_spinner_tick,
                view.generation_stage,
            ));
        }

        if let Some(error) = view.error {
            card = card.child(self.render_form_error(view, error));
        }

        card
    }

    fn render_cost_preview(&self, view: &DeliveryFormView<'_>) -> Option<gpui::Div> {
        if let Some(estimate) = view.cost_estimate {
            let anchor_rate = self
                .public_broadcaster_anchor_cache
                .cached_rate(view.asset.chain_id, estimate.fee_token);
            Some(render_public_broadcaster_cost_estimate(
                view.root.clone(),
                view.key,
                view.kind,
                view.asset,
                estimate,
                anchor_rate,
                Some(&self.effective_token_registry),
                &self.public_broadcaster_anchor_cache,
                view.transaction_fee_breakdown_open,
                view.estimating_cost,
                view.custom_fee_amount,
                !view.generating && !view.gateway_executing,
            ))
        } else {
            public_broadcaster_cost_status(view.cost_estimate_pending, view.estimating_cost).map(
                |status| render_public_broadcaster_cost_status(self.unshield_spinner_tick, status),
            )
        }
    }

    fn render_form_error(&self, view: &DeliveryFormView<'_>, error: &str) -> gpui::Div {
        let alert = Alert::error(
            view.element_id("form-error"),
            format_form_error_for_asset(
                error,
                view.asset,
                view.selected_fee_token,
                Some(&self.effective_token_registry),
            ),
        )
        .small();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w(px(0.0)).child(alert))
            .children(
                view.estimate_retry_visible()
                    .then(|| view.estimate_retry_button()),
            )
    }
}

impl DeliveryFormView<'_> {
    fn element_id(&self, action: &str) -> SharedString {
        delivery_element_id(self.key, self.kind, action)
    }

    /// Debug selectors the routing tests click by.
    const fn selector(&self, send: &'static str, unshield: &'static str) -> &'static str {
        match self.kind {
            DeliveryFormKind::Send => send,
            DeliveryFormKind::Unshield => unshield,
        }
    }

    /// The form's Generate action, shared by the button and both Enter handlers.
    fn generate_handler(&self) -> impl Fn(&mut Window, &mut App) + Clone + use<> {
        let root = self.root.clone();
        let (kind, key) = (self.kind, self.key);
        move |window, cx| {
            root.update(cx, |root, cx| match kind {
                DeliveryFormKind::Send => root.generate_send_calldata_from_form(key, window, cx),
                DeliveryFormKind::Unshield => {
                    root.generate_unshield_calldata_from_form(key, window, cx);
                }
            });
        }
    }

    const fn broadcast_generating(&self) -> bool {
        self.generating
            && matches!(
                self.delivery_mode,
                DeliveryMode::PublicBroadcaster | DeliveryMode::SelfBroadcast
            )
    }

    fn recipient_field(&self, label: &'static str, options: &[RecipientOption]) -> gpui::Div {
        labeled_field(
            label,
            render_recipient_picker(
                self.root.clone(),
                self.key,
                self.kind,
                self.recipient_input,
                self.recipient_value,
                self.recipient_suggestions_open,
                self.recipient_suggestion_index,
                self.recipient_suggestions_scroll,
                options,
                self.generating,
            ),
        )
        .flex_1()
        .min_w(px(0.0))
    }

    /// `max` is the Max label's amount and whether it is only an upper bound.
    fn amount_field(&self, max: (U256, bool), submit_disabled: bool) -> gpui::Div {
        let (max_amount, upper_bound) = max;
        let max_amount = private_action_metric_display_amount(max_amount, self.asset.decimals);
        let unit_hint = if self.asset.decimals.is_some() {
            format!("{} amount", self.asset.label)
        } else {
            "Raw base units for this unknown token".to_string()
        };
        render_private_action_amount_field(
            self.root.clone(),
            self.key,
            self.kind,
            unit_hint,
            Some(if upper_bound {
                format!("≈ {max_amount}")
            } else {
                max_amount
            }),
            self.generating,
            private_action_amount_input(
                self.amount_input,
                self.generating,
                !submit_disabled,
                self.generate_handler(),
            ),
        )
        .w(px(220.0))
    }

    /// A pin conflict keeps submit enabled: submitting is how the user reaches the override.
    fn public_broadcaster_submit_disabled(
        &self,
        fee_token_options: &[PublicBroadcasterFeeTokenOption],
    ) -> bool {
        public_broadcaster_submit_disabled_for_fee_token_options(
            fee_token_options,
            self.selected_fee_token,
        ) || (!broadcaster_estimate_allows_submission(
            self.cost_estimate,
            self.cost_estimate_pending || self.estimating_cost,
        ) && !self.pin_conflict_awaits_choice)
    }

    const fn submit_label(&self) -> &'static str {
        if self.generating {
            "Preparing..."
        } else if self.submitted {
            "Submitted"
        } else {
            match self.delivery_mode {
                DeliveryMode::PublicBroadcaster => "Submit via broadcaster",
                DeliveryMode::SelfBroadcast => "Self-broadcast",
                DeliveryMode::ManualCalldata => "Generate calldata",
            }
        }
    }

    fn submit_button(&self, submit_disabled: bool) -> Button {
        let selector = self.selector("send-generate", "unshield-generate");
        let generate = self.generate_handler();
        app_button(self.element_id("generate"), self.submit_label())
            .debug_selector(move || selector.into())
            .primary()
            .loading(self.generating)
            .disabled(submit_disabled)
            .tooltip(if self.generation_ready {
                "Prepare private transaction"
            } else {
                "Generation is available after wallet sync finishes"
            })
            .on_click(move |_event, window, cx| generate(window, cx))
    }

    fn custom_fee_row_visible(&self) -> bool {
        self.delivery_mode == DeliveryMode::PublicBroadcaster
            && self.custom_fee_amount.is_some()
            && (self.cost_estimate.is_none() || self.error.is_some())
            && !self.generating
            && !self.gateway_executing
            && !self.has_result
    }

    /// Nothing else restarts a failed or abandoned estimate until an input changes.
    fn estimate_retry_visible(&self) -> bool {
        self.delivery_mode == DeliveryMode::PublicBroadcaster
            && self.cost_estimate.is_none()
            && !self.cost_estimate_pending
            && !self.estimating_cost
            && !self.generating
            && !self.has_result
            && !self.pin_conflict_awaits_choice
    }

    fn estimate_retry_button(&self) -> Button {
        let selector = self.selector("send-estimate-retry", "unshield-estimate-retry");
        let root = self.root.clone();
        let (kind, key) = (self.kind, self.key);
        app_button(self.element_id("estimate-retry"), "Retry")
            .debug_selector(move || selector.into())
            .outline()
            .small()
            .flex_none()
            .on_click(move |_event, _window, cx| {
                root.update(cx, |root, cx| {
                    root.schedule_public_broadcaster_cost_estimate(kind, key, cx);
                });
            })
    }
}

fn render_sponsored_self_broadcast_result(
    result: &wallet_ops::DesktopSponsoredSelfBroadcastResult,
) -> gpui::Div {
    let authorization = result.prepared.authorization;
    let status = match &result.outcome {
        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) if receipt.status => {
            "Sponsored transaction confirmed"
        }
        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(_) => {
            "Sponsored transaction reverted"
        }
        SponsoredSelfBroadcastSessionOutcome::Stopped { .. } => "Sponsored retries stopped locally",
    };
    let mut content = div()
        .flex()
        .flex_col()
        .gap_1()
        .p(px(10.0))
        .child(app_strong_text(status))
        .child(app_muted_text(format!(
            "Builder payment: {} native base units",
            authorization.builder_payment
        )))
        .child(app_muted_text(format!(
            "Private wrapped-native spend: {} (protocol fee {})",
            authorization.gross_wrapped_native_spend, authorization.protocol_fee
        )))
        .child(app_muted_text(format!(
            "Outer gas limit: {} · signer: {}",
            authorization.transaction_gas_limit,
            authorization.signer.to_checksum(None)
        )))
        .child(app_muted_text(
            "Priority fee is paid to the block builder in addition to the private builder payment.",
        ));
    if let SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) = &result.outcome {
        content = content
            .child(app_muted_text(format!("Transaction: {}", receipt.tx_hash)))
            .child(app_muted_text(format!("Block: {}", receipt.block_number)));
    }
    content
}
