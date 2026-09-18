use std::time::{Duration, Instant};

use gpui::{Context, ParentElement as _, Styled as _, Window, div};
use wallet_ops::{
    PublicBroadcasterCandidate, PublicBroadcasterSelection, settings::ExecutorProfile,
};

use super::super::StealthAccountsView;
use super::{ExecutorAsset, ExecutorRecoveryFeeEstimate, SelfBroadcastGasFeeQuote};
use crate::root::broadcaster_picker::{
    BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL, BroadcasterChoice,
    BroadcasterPickerFeeEstimateContext, BroadcasterPickerTarget, broadcaster_candidate_label,
    selected_broadcaster_label,
};
use crate::root::public_broadcaster::{
    public_broadcaster_candidates_for_route, public_broadcaster_fee_token_options_from_snapshot,
    resolve_selected_public_broadcaster_fee_token,
};
use crate::root::public_broadcaster_cost::PublicBroadcasterFeeDisplay;
use crate::root::{COST_ESTIMATE_DEBOUNCE, DeliveryMode};
use ui::controls::app_muted_text;
use ui::private_action::{BroadcasterSettings, BroadcasterSettingsEvent};

pub(in crate::root) struct RecoveryPickerContext {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) token: alloy::primitives::Address,
    pub(in crate::root) choice: BroadcasterChoice,
    pub(in crate::root) candidates: Vec<PublicBroadcasterCandidate>,
    pub(in crate::root) allow_out_of_range: bool,
    pub(in crate::root) favorites_only: bool,
    pub(in crate::root) busy: bool,
    pub(in crate::root) estimating: bool,
    pub(in crate::root) fee_context: Option<BroadcasterPickerFeeEstimateContext>,
}

enum RecoveryEstimate {
    Native(SelfBroadcastGasFeeQuote),
    Broadcaster(Box<ExecutorRecoveryFeeEstimate>),
}

pub(super) fn same_offer(
    left: &PublicBroadcasterCandidate,
    right: &PublicBroadcasterCandidate,
) -> bool {
    left.chain_id == right.chain_id
        && left.railgun_address == right.railgun_address
        && left.token == right.token
        && left.fees_id == right.fees_id
        && left.fee == right.fee
        && left.fee_expiration == right.fee_expiration
        && left.viewing_public_key == right.viewing_public_key
        && left.required_poi_list_keys == right.required_poi_list_keys
        && left.relay_adapt_7702 == right.relay_adapt_7702
}

impl StealthAccountsView {
    pub(in crate::root::stealth_accounts) fn start_recovery_updates(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.ensure_recovery_network(cx);
        self.refresh_recovery_controls(window, cx);
        self.recovery.refresh_task = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL)
                    .await;
                let active = view
                    .update_in(cx, |view, window, cx| {
                        if !view.recovery.open || !view.session_is_current(cx) {
                            return false;
                        }
                        view.refresh_recovery_controls(window, cx);
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        }));
    }

    pub(in crate::root::stealth_accounts) fn close_recovery(&mut self) {
        self.recovery.open = false;
        self.recovery.refresh_task = None;
        self.recovery.estimate_task = None;
        self.recovery.estimate_revision = self.recovery.estimate_revision.wrapping_add(1);
        self.pending_authorization = None;
    }

    pub(super) fn ensure_recovery_network(&self, cx: &mut Context<'_, Self>) {
        if !self.recovery.native_funding {
            let _ = self.root.update(cx, |root, cx| {
                root.ensure_waku_for_delivery(DeliveryMode::PublicBroadcaster, cx);
                root.public_broadcaster_anchor_refresh.wake();
            });
        }
    }

    pub(in crate::root::stealth_accounts) fn refresh_recovery_controls(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.recovery.open || !self.session_is_current(cx) {
            return;
        }
        self.sync_recovery_assets(window, cx);
        if self.job.is_some() || self.pending_authorization.is_some() {
            return;
        }
        if self.recovery.native_funding && !self.recovery_has_native_balance() {
            self.recovery.native_funding = false;
            self.invalidate_recovery();
            self.ensure_recovery_network(cx);
        }
        self.refresh_recovery_broadcasters(cx);
        if self.recovery.prepared.is_none()
            && self.recovery.estimate_task.is_none()
            && Instant::now() >= self.recovery.next_estimate
        {
            self.schedule_recovery_estimate(cx);
        }
        cx.notify();
    }

    fn refresh_recovery_broadcasters(&mut self, cx: &Context<'_, Self>) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let root = root.read(cx);
        let profile = self
            .records
            .iter()
            .find(|record| Some(record.operation()) == self.selected)
            .and_then(|record| ExecutorProfile::accepted(self.session.chain_id, record.delegate()));
        let policy = root.public_broadcaster_fee_policy(self.recovery.allow_out_of_range);
        let trust = root.public_broadcaster_trust_filter(self.recovery.favorites_only);
        let rows = root.monitor_fee_rows();
        let options = profile
            .and_then(|profile| {
                root.chain_states
                    .get(&self.session.chain_id)
                    .and_then(|state| state.snapshot())
                    .map(|snapshot| {
                        public_broadcaster_fee_token_options_from_snapshot(
                            snapshot,
                            &rows,
                            None,
                            Some(profile),
                            policy,
                            &trust,
                            Some(&root.effective_token_registry),
                            |token| {
                                root.public_broadcaster_anchor_cache
                                    .cached_rate(self.session.chain_id, token)
                            },
                        )
                    })
            })
            .unwrap_or_default();
        let preferred = match self.recovery.asset {
            Some(ExecutorAsset::Erc20(token)) => token,
            _ => root
                .effective_chain_configs
                .get(&self.session.chain_id)
                .and_then(|chain| chain.wrapped_native_token.as_ref())
                .and_then(|token| token.parse().ok())
                .unwrap_or_default(),
        };
        let token = (!options.is_empty()).then(|| {
            resolve_selected_public_broadcaster_fee_token(
                self.recovery.fee_token.unwrap_or(preferred),
                preferred,
                &options,
            )
        });
        let candidates = token
            .zip(profile)
            .map(|(token, profile)| {
                wallet_ops::fee_policy_eligible_public_broadcasters(
                    &public_broadcaster_candidates_for_route(
                        &rows,
                        self.session.chain_id,
                        token,
                        None,
                        Some(profile),
                        policy,
                        root.public_broadcaster_anchor_cache
                            .cached_rate(self.session.chain_id, token),
                        &trust,
                    ),
                    policy,
                )
            })
            .unwrap_or_default();
        let quote_changed = self
            .recovery
            .estimate_candidate
            .as_ref()
            .is_some_and(|quoted| {
                !candidates
                    .iter()
                    .any(|candidate| same_offer(quoted, candidate))
            });
        let token_changed = self.recovery.fee_token != token;
        let choice_changed = self
            .recovery
            .selected_broadcaster
            .as_ref()
            .is_some_and(|address| {
                !candidates
                    .iter()
                    .any(|candidate| &candidate.railgun_address == address)
            });
        if choice_changed {
            self.recovery.selected_broadcaster = None;
        }
        self.recovery.fee_options = options;
        self.recovery.fee_token = token;
        self.recovery.candidates = candidates;
        if !self.recovery.native_funding && (quote_changed || token_changed || choice_changed) {
            self.invalidate_recovery();
            self.recovery.next_estimate = Instant::now();
        }
    }

    pub(in crate::root::stealth_accounts) fn schedule_recovery_estimate(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.recovery.open
            || !self.session_is_current(cx)
            || self.job.is_some()
            || self.pending_authorization.is_some()
        {
            return;
        }
        let Some(operation) = self.selected else {
            return;
        };
        let Some(asset) = self.recovery.asset else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let root = root.read(cx);
        let Some(chain) = root
            .effective_chain_configs
            .get(&self.session.chain_id)
            .cloned()
        else {
            return;
        };
        let http = root.http.clone();
        let native = self.recovery.native_funding;
        let candidate = if native {
            None
        } else {
            let selection = self.recovery.selected_broadcaster.as_ref().map_or(
                PublicBroadcasterSelection::Random,
                |address| PublicBroadcasterSelection::Specific {
                    railgun_address: address.clone(),
                },
            );
            let Ok(candidate) = wallet_ops::select_public_broadcaster_with_policy_and_trust(
                &self.recovery.candidates,
                &selection,
                root.public_broadcaster_fee_policy(self.recovery.allow_out_of_range),
                &root.public_broadcaster_trust_filter(self.recovery.favorites_only),
            ) else {
                return;
            };
            Some(candidate)
        };
        self.recovery.estimate_revision = self.recovery.estimate_revision.wrapping_add(1);
        let revision = self.recovery.estimate_revision;
        self.recovery.estimate_candidate.clone_from(&candidate);
        self.recovery.fee_estimate = None;
        self.recovery.fee_error = None;
        let owner = self.owner.clone();
        let session = self.session.clone();
        let runtime = self.runtime.clone();
        self.recovery.estimate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(COST_ESTIMATE_DEBOUNCE).await;
            let result = runtime
                .spawn(async move {
                    if let Some(candidate) = candidate {
                        owner
                            .estimate_recovery_fee(operation, asset, &session, candidate)
                            .await
                            .map(|estimate| RecoveryEstimate::Broadcaster(Box::new(estimate)))
                    } else {
                        wallet_ops::quote_desktop_self_broadcast_gas_fee(
                            chain.chain_id,
                            Some(&chain),
                            &http,
                        )
                        .await
                        .map(RecoveryEstimate::Native)
                    }
                })
                .await;
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = view.update(cx, |view, cx| {
                if !view.recovery.open
                    || !view.session_is_current(cx)
                    || view.recovery.estimate_revision != revision
                {
                    return;
                }
                view.recovery.estimate_task = None;
                view.recovery.next_estimate = Instant::now()
                    + if result.is_ok() {
                        Duration::from_secs(30)
                    } else {
                        Duration::from_secs(5)
                    };
                match result {
                    Ok(RecoveryEstimate::Native(quote)) => view.recovery.gas_quote = Some(quote),
                    Ok(RecoveryEstimate::Broadcaster(quote)) => {
                        view.recovery.fee_estimate = Some(*quote);
                    }
                    Err(error) => view.recovery.fee_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(in crate::root) fn recovery_picker_context(&self) -> Option<RecoveryPickerContext> {
        if !self.recovery.open || self.recovery.native_funding {
            return None;
        }
        Some(RecoveryPickerContext {
            chain_id: self.session.chain_id,
            token: self.recovery.fee_token?,
            choice: self
                .recovery
                .selected_broadcaster
                .clone()
                .map_or(BroadcasterChoice::Random, |railgun_address| {
                    BroadcasterChoice::Specific { railgun_address }
                }),
            candidates: self.recovery.candidates.clone(),
            allow_out_of_range: self.recovery.allow_out_of_range,
            favorites_only: self.recovery.favorites_only,
            busy: self.job.is_some(),
            estimating: self.recovery.estimate_task.is_some(),
            fee_context: self
                .recovery
                .fee_estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from),
        })
    }

    pub(in crate::root) fn set_recovery_allow_out_of_range(
        &mut self,
        checked: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.recovery.open || self.job.is_some() {
            return;
        }
        self.recovery.allow_out_of_range = checked;
        self.invalidate_recovery();
        self.refresh_recovery_broadcasters(cx);
        self.schedule_recovery_estimate(cx);
        cx.notify();
    }

    pub(in crate::root) fn choose_recovery_broadcaster(
        &mut self,
        address: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.recovery.open || self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_recovery_broadcasters(cx);
        if !self
            .recovery
            .candidates
            .iter()
            .any(|candidate| candidate.railgun_address == address)
        {
            return;
        }
        self.recovery.selected_broadcaster = Some(address);
        self.invalidate_recovery();
        self.schedule_recovery_estimate(cx);
        self.focus_recovery_amount(window, cx);
        cx.notify();
    }

    pub(super) fn render_recovery_broadcaster_settings(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let view = cx.entity();
        let fee_view = view.clone();
        let choice = self
            .recovery
            .selected_broadcaster
            .clone()
            .map_or(BroadcasterChoice::Random, |railgun_address| {
                BroadcasterChoice::Specific { railgun_address }
            });
        let fee_token = crate::root::private_action::fee_token_selector(
            "stealth-recovery-fee-token".into(),
            &self.recovery.fee_options,
            self.recovery.fee_token.unwrap_or_default(),
            self.job.is_some(),
            move |token, _, cx| {
                fee_view.update(cx, |view, cx| {
                    view.recovery.fee_token = Some(token);
                    view.invalidate_recovery();
                    view.refresh_recovery_broadcasters(cx);
                    view.schedule_recovery_estimate(cx);
                    cx.notify();
                });
            },
        );
        let mut content =
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(ui::private_action::broadcaster_settings(
                    "stealth-broadcaster-settings",
                    BroadcasterSettings {
                        allow_out_of_range: self.recovery.allow_out_of_range,
                        favorites_only: self.recovery.favorites_only,
                        random_selected: self.recovery.selected_broadcaster.is_none(),
                        specific_label: selected_broadcaster_label(
                            &choice,
                            &self.recovery.candidates,
                        ),
                        candidate_count: self.recovery.candidates.len(),
                        disabled: self.job.is_some(),
                    },
                    fee_token,
                    None,
                    move |event, window, cx| {
                        view.update(cx, |view, cx| {
                            if matches!(event, BroadcasterSettingsEvent::ChooseSpecific) {
                                let Some(token) = view.recovery.fee_token else {
                                    return;
                                };
                                let target = BroadcasterPickerTarget::Recovery(cx.weak_entity());
                                let chain_id = view.session.chain_id;
                                let _ = view.root.update(cx, |root, cx| {
                                    root.open_broadcaster_picker_for_target(
                                        target, "Recovery", chain_id, token, window, cx,
                                    );
                                });
                                return;
                            }
                            match event {
                                BroadcasterSettingsEvent::Random => {
                                    view.recovery.selected_broadcaster = None;
                                }
                                BroadcasterSettingsEvent::AllowOutOfRange(value) => {
                                    view.recovery.allow_out_of_range = value;
                                }
                                BroadcasterSettingsEvent::FavoritesOnly(value) => {
                                    view.recovery.favorites_only = value;
                                }
                                BroadcasterSettingsEvent::ChooseSpecific => unreachable!(),
                            }
                            view.invalidate_recovery();
                            view.refresh_recovery_broadcasters(cx);
                            view.schedule_recovery_estimate(cx);
                            cx.notify();
                        });
                    },
                ));
        if let Some(estimate) = &self.recovery.fee_estimate {
            if let Some(root) = self.root.upgrade() {
                let root = root.read(cx);
                let cache = &root.public_broadcaster_anchor_cache;
                let display = PublicBroadcasterFeeDisplay {
                    broadcaster: estimate.broadcaster(),
                    registry: Some(&root.effective_token_registry),
                    chain_id: self.session.chain_id,
                    fee_token: estimate.broadcaster().token,
                    fee_amount: estimate.fee_amount(),
                    gas_limit: estimate.gas_limit(),
                    min_gas_price: estimate.min_gas_price(),
                    fee_anchor_rate: cache
                        .cached_rate(self.session.chain_id, estimate.broadcaster().token),
                };
                let fee_view = cx.entity();
                let refresh_view = cx.entity();
                content = content.child(ui::private_action::estimated_outcome(
                    broadcaster_candidate_label(estimate.broadcaster()),
                    Vec::new(),
                    ui::private_action::transaction_fee_breakdown(
                        "stealth-recovery-fee-breakdown",
                        display.fee_amount_with_usd(cache),
                        display.fee_rows(cache),
                        display.gas_value(),
                        self.recovery.fee_breakdown_open,
                        None,
                        move |open, _, cx| {
                            fee_view.update(cx, |view, cx| {
                                view.recovery.fee_breakdown_open = open;
                                cx.notify();
                            });
                        },
                    ),
                    "Conservative estimate including shielding, token approvals, and the private fee payment. Final gas is checked before submission.".into(),
                    "stealth-recovery-refresh-estimate",
                    self.recovery.estimate_task.is_some() || self.job.is_some() || self.pending_authorization.is_some(),
                    move |_, cx| {
                        refresh_view.update(cx, |view, cx| {
                            if !view.recovery.open || !view.session_is_current(cx)
                                || view.job.is_some() || view.pending_authorization.is_some()
                            {
                                return;
                            }
                            view.invalidate_recovery();
                            view.refresh_recovery_broadcasters(cx);
                            view.schedule_recovery_estimate(cx);
                            cx.notify();
                        });
                    },
                ));
            }
        } else {
            let status = if let Some(error) = &self.recovery.fee_error {
                error.as_str()
            } else if self.recovery.estimate_task.is_some() {
                "Estimating broadcaster fee…"
            } else if self.recovery.fee_options.is_empty() {
                "No spendable private fee tokens. Add private funds or fund this account's native gas."
            } else {
                "Waiting for a compatible broadcaster for the selected fee token."
            };
            content = content.child(app_muted_text(status.to_owned()).whitespace_normal());
        }
        content
    }
}
