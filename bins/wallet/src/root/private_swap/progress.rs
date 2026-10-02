//! The Private tab's swap card, each swap's order detail, and the actions its state allows:
//! cancel an open order, retry after an attempt ended, recover stranded funds through Stealth
//! accounts, and remove a swap that ended from the Private tab.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    App, ClickEvent, Context, Entity, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, img, prelude::FluentBuilder as _, px, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{ButtonVariant, ButtonVariants},
    collapsible::Collapsible,
    dialog::DialogButtonProps,
    tooltip::Tooltip,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::theme;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorPaidRecoveryRequest,
    ExecutorRecoveryFeeEstimate, PublicBroadcasterCandidate, PublicBroadcasterSelection,
    SwapOrderState, WakuDeliveryClient,
    vault::{
        BridgeDelivery, BridgeOrderTerms, BridgeProvider, ExecutorOperationId, ExecutorRecord,
        SwapBridgeOutcome, SwapDelivery, SwapOrderRecord, SwapPreHookDeathCause,
        SwapSubmissionStatus,
    },
};

use super::dialog::{SwapDialogView, settled_by_cow};
use super::form::{broadcaster_result_problem, format_gwei, gas_share_name, network_name};
use super::model::{
    SwapActions, SwapOrderGroup, SwapSetupProgress, SwapStage, SwapStep, SwapStepAccount,
    bridge_sent_amount, needs_executed_fee, private_delivery_credit, provider_name,
    record_swap_ranges, swap_actions, swap_order_group, swap_order_stage, swap_outcome,
    swap_private_delivery, swap_private_minimum, swap_steps, swap_valid_to, swaps_card_line,
};
use super::{
    PrivateSwapsView, SWAP_BROADCASTER_REPUBLISH_INTERVAL, SWAP_BROADCASTER_RESPONSE_TIMEOUT,
    SwapAction, SwapJobKind, local_date_time_label, local_time_label, now_unix, short_receiver,
    swap_delivery, swap_recovery_token, swap_tokens,
};
use crate::assets::WalletIconSource;
use crate::root::broadcaster_picker::broadcaster_candidate_label;
use crate::root::dialog_max_height;
use crate::root::public_action::PublicActionStepStatus;
use crate::root::spend_authorization::{SpendAuthorizationSummary, SpendAuthorizationSummaryRow};
use crate::root::stealth_accounts::StealthAccountTarget;
use crate::root::submission_progress::{
    SubmissionProgressGroup, SubmissionProgressStep, SubmissionProgressSubstep,
    render_submission_progress_groups,
};
use crate::root::utxo::short_hash;

/// A broadcaster's fee for an early cancellation, estimated before the confirmation.
pub(super) struct CancelQuote {
    operation: ExecutorOperationId,
    estimate: ExecutorRecoveryFeeEstimate,
}

/// The cancellation the user approved.
#[derive(Clone)]
pub(super) struct CancelApproval {
    pub(super) operation: ExecutorOperationId,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
    waku: Arc<WakuDeliveryClient>,
}

impl Render for PrivateSwapsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl gpui::IntoElement {
        self.render_card(&cx.entity(), cx).map_or_else(
            || gpui::Empty.into_any_element(),
            gpui::IntoElement::into_any_element,
        )
    }
}

impl PrivateSwapsView {
    pub(in crate::root) fn has_shown_swaps(&self) -> bool {
        self.shown_swaps().next().is_some()
    }

    /// The one card for every unfinished swap, in the Private tab's pending-status slot.
    fn render_card(&self, view: &Entity<Self>, cx: &App) -> Option<gpui::Div> {
        if !self.session_is_current(cx) {
            return None;
        }
        let swaps = self
            .shown_swaps()
            .map(|(record, stage)| (stage, self.labels(record, cx)))
            .collect::<Vec<_>>();
        let line = swaps_card_line(&swaps)?;
        let view = view.clone();
        let action = app_button("wallet-private-swaps-details", "Details…")
            .ghost()
            .xsmall()
            .compact()
            .on_click(move |_, window, cx| {
                view.update(cx, |view, cx| view.open_details(window, cx));
            });
        let detail = Some(SharedString::from(line.detail));
        Some(if line.attention {
            ui::private_assets::private_attention_status(line.title, detail, action)
        } else {
            ui::private_assets::private_pending_status(line.title, detail, action)
        })
    }

    /// One shown swap opens its detail; several open My orders, filtered to Open.
    pub(super) fn open_details(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let shown = self
            .shown_swaps()
            .map(|(record, _)| record.operation())
            .collect::<Vec<_>>();
        match shown.as_slice() {
            [] => {}
            [operation] => self.show_detail(*operation, window, cx),
            _ => {
                self.orders_filter = Some(SwapOrderGroup::Open);
                self.show_view(SwapDialogView::Orders, window, cx);
            }
        }
    }

    pub(super) fn progress_title(&self, operation: ExecutorOperationId, cx: &App) -> String {
        self.record(operation).map_or_else(
            || "Swap".into(),
            |record| format!("Swap {}", self.labels(record, cx).pair),
        )
    }

    /// One swap's order detail: its steps, facts and outcome, and a footer with the actions its
    /// state allows.
    pub(super) fn render_detail(
        &self,
        operation: ExecutorOperationId,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let Some(record) = self.record(operation) else {
            // An approved setup reserves the stealth account first.
            let reserving = self
                .job
                .as_ref()
                .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
            let error = self
                .tracking
                .get(&operation)
                .and_then(|tracking| tracking.error.clone())
                .or_else(|| self.error.clone());
            return (
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(if reserving {
                        "Reserving the swap's stealth account…"
                    } else if error.is_some() {
                        "The swap wasn't set up. Nothing was paid."
                    } else {
                        "This swap is no longer saved in this wallet."
                    }))
                    .children(error.map(|error| {
                        app_muted_text(error)
                            .text_color(rgb(theme::DANGER))
                            .whitespace_normal()
                    })),
                None,
            );
        };
        let pending = self.pending_order(record);
        let stage = self.progress_stage(record);
        let stopped = record.is_swap_setup_stopped();
        let labels = self.labels(record, cx);
        let tracking = self.tracking.get(&operation);
        let job = self
            .job
            .as_ref()
            .filter(|job| job.operation == operation)
            .map(|job| job.kind);
        let cancelling = tracking.is_some_and(|tracking| tracking.cancelling)
            || job == Some(SwapJobKind::Cancel);
        let past_valid_to = swap_valid_to(record).is_some_and(|valid_to| now_unix() > valid_to);
        // A stopped setup can't issue more swap work. Its account stays in Stealth accounts.
        let mut actions = if stopped {
            SwapActions::default()
        } else {
            swap_actions(stage, past_valid_to)
        };
        if let Some(problem) = self.setup_retry_problem(record) {
            actions.resume = actions.resume.map(|_| Err(problem));
        }
        // A private Bridge swap whose own account is set up waits for its destination
        // account's setup, which the swap's own setup actions don't cover. A setup there that
        // failed or was never sent is retried by itself.
        let destination_setup = swap_private_delivery(record)
            .filter(|_| {
                !stopped
                    && pending.is_none()
                    && matches!(self.stage(record), SwapStage::Ready | SwapStage::Approved)
            })
            .map(|delivery| (delivery, self.destination_setup_progress(record, delivery)))
            .filter(|(_, progress)| *progress != SwapSetupProgress::Done);
        let retry_destination = destination_setup.and_then(|(delivery, progress)| {
            matches!(
                progress,
                SwapSetupProgress::NotSent | SwapSetupProgress::Failed
            )
            .then_some(delivery.destination_chain)
        });
        if destination_setup.is_some() {
            actions.resume = None;
        }
        if cancelling {
            actions.cancel = actions
                .cancel
                .map(|_| Err("A cancellation is on its way. Its outcome shows here."));
        }
        // The reported settlement already ran the pre-hook, so a cancellation could only fail
        // or waste its fee. Observation still owns the stage; an unfilled order expires anyway.
        if labels.fill_hint.is_some() {
            actions.cancel = None;
        }
        let setup_stage = tracking
            .and_then(|tracking| tracking.setup_stage.as_ref())
            .map(|receiver| *receiver.borrow());
        let steps = swap_steps(stage, &labels)
            .into_iter()
            .enumerate()
            .map(|(index, mut step)| {
                // A setup step with sub-steps shows each account's progress on its own.
                let setup = index == 0 && step.children.is_empty();
                let detail = if setup
                    && stage == SwapStage::SetupSubmitting
                    && let Some(setup_stage) = setup_stage
                {
                    setup_stage.label().to_owned()
                } else if setup
                    && stage == SwapStage::SetupPending
                    && let Some(detail) = self.setup_confirmation_detail(record, cx)
                {
                    detail
                } else if index == 1 && cancelling && step.status == PublicActionStepStatus::Pending
                {
                    "Cancelling… The order can still fill until the cancellation is final.".into()
                } else if index == 1 && job == Some(SwapJobKind::Order) {
                    step.status = PublicActionStepStatus::Pending;
                    "Preparing and placing the order…".into()
                } else if index == 1 && job == Some(SwapJobKind::Requote) {
                    step.status = PublicActionStepStatus::Pending;
                    "Checking the approved terms…".into()
                } else {
                    step.detail.clone()
                };
                progress_group(
                    &step,
                    detail,
                    format!("swap-step-{}-{index}", operation.opaque_id()),
                )
            });
        let error = tracking
            .and_then(|tracking| tracking.error.clone())
            .or_else(|| self.error.clone());
        let note: Option<SharedString> = if stopped {
            Some(
                "This setup was stopped, so no order will be placed. Its stealth account stays in Stealth accounts."
                    .into(),
            )
        } else if record.is_hidden() && actions.dismiss {
            Some("Removed from the Private tab. Tracking continues in My orders.".into())
        } else if let Some(chain_id) = retry_destination {
            Some(
                format!(
                    "Retry sends only the setup on {} again. The fee already paid on {} isn't paid again. Nothing was unshielded.",
                    network_name(chain_id),
                    network_name(self.session.chain_id)
                )
                .into(),
            )
        } else {
            progress_note(stage).map(Into::into)
        };
        let latest = pending
            .is_none()
            .then(|| record_swap_ranges(record).pop())
            .flatten();
        let order = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .filter(|_| pending.is_none());
        let checked = tracking
            .and_then(|tracking| tracking.status_checked)
            .filter(|(uid, _)| {
                job.is_none()
                    && error.is_none()
                    && stage.is_observed()
                    && order.is_some_and(|order| order.uid() == *uid)
            })
            .map(|(_, at)| {
                format!(
                    "Checked at {}. The outcome is not confirmed yet. Check again after more blocks arrive.",
                    local_time_label(at),
                )
            });
        let group = swap_order_group(stage, stopped, record.is_hidden());
        let minimum = pending
            .map(|pending| (pending.buy, pending.private_minimum))
            .or_else(|| {
                swap_tokens(record)
                    .zip(swap_private_minimum(record))
                    .filter(|_| group == SwapOrderGroup::Open)
                    .map(|((_, buy), minimum)| (buy, minimum))
            });
        let started = pending
            .map(|pending| ("Started", pending.started_at))
            .or_else(|| self.swap_started(record, latest.as_ref(), cx));
        let delivery = pending.map_or_else(|| swap_delivery(record), |pending| pending.delivery);
        let outcome =
            order.and_then(|order| self.render_outcome(record, order, stage, started, cx));
        let not_filled = order.and_then(|order| self.render_not_filled(record, order, stage, cx));
        let facts = match delivery {
            SwapDelivery::Bridge(bridge) => {
                Some(self.render_bridge_facts(record, order, bridge, stage, started, cx))
            }
            _ if outcome.is_none() => self.render_facts(order, delivery, minimum, started, cx),
            _ => None,
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .children(
                shows_steps(stage, outcome.is_some())
                    .then(|| render_submission_progress_groups(steps)),
            )
            .children(not_filled)
            .children(facts)
            .children(outcome)
            .children(
                latest
                    .and_then(|range| earlier_attempts_note(record, range))
                    .map(|note| app_muted_text(note).whitespace_normal()),
            )
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
            .children(checked.map(|message| app_muted_text(message).whitespace_normal()))
            .children(error.map(|error| {
                app_muted_text(error)
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal()
            }));
        (
            body,
            Some(self.render_progress_actions(
                operation,
                stage,
                actions,
                retry_destination,
                job,
                cx,
            )),
        )
    }

    /// An earlier swap on a reused stealth account, read only: its steps, facts and outcome from
    /// its own orders, and a footer that only closes. The account's latest swap owns every action.
    pub(super) fn render_past_detail(
        &self,
        operation: ExecutorOperationId,
        first: usize,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let close = app_button("swap-progress-close", "Close")
            .small()
            .flex_none()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_swap_dialog(window, cx);
            }));
        let footer = div()
            .w_full()
            .flex()
            .items_center()
            .child(div().flex_1())
            .child(close);
        let Some((record, range, order)) = self.past_swap(operation, first) else {
            return (
                app_muted_text("This swap is no longer saved in this wallet."),
                Some(footer),
            );
        };
        let stage = swap_order_stage(record, order);
        let labels = self.past_labels(record, order, cx);
        let steps = swap_steps(stage, &labels)
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                let detail = step.detail.clone();
                progress_group(
                    &step,
                    detail,
                    format!("swap-step-{}-{first}-{index}", operation.opaque_id()),
                )
            });
        // Recovery acts on the account, which its latest swap and Stealth accounts offer.
        let recovery = (stage.needs_recovery() || self.stage(record).needs_recovery()).then_some(
            "This stealth account needs recovery. Recover it from its latest swap or from Stealth accounts.",
        );
        let started = self.swap_started(record, Some(&range), cx);
        let outcome = self.render_outcome(record, order, stage, started, cx);
        let not_filled = self.render_not_filled(record, order, stage, cx);
        let facts = match order.delivery() {
            SwapDelivery::Bridge(bridge) => {
                Some(self.render_bridge_facts(record, Some(order), bridge, stage, started, cx))
            }
            delivery if outcome.is_none() => {
                self.render_facts(Some(order), delivery, None, started, cx)
            }
            _ => None,
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .children(
                shows_steps(stage, outcome.is_some())
                    .then(|| render_submission_progress_groups(steps)),
            )
            .children(not_filled)
            .children(facts)
            .children(outcome)
            .children(
                earlier_attempts_note(record, range)
                    .map(|note| app_muted_text(note).whitespace_normal()),
            )
            .children(recovery.map(|note| app_muted_text(note).whitespace_normal()));
        (body, Some(footer))
    }

    /// The order ID, who settles it, a Public address swap's receiver, the minimum while it can
    /// fill, and when the swap started.
    fn render_facts(
        &self,
        order: Option<&SwapOrderRecord>,
        delivery: SwapDelivery,
        minimum: Option<(Address, U256)>,
        started: Option<(&'static str, u64)>,
        cx: &App,
    ) -> Option<gpui::Div> {
        let mut rows = Vec::new();
        if let Some(order) = order {
            let order_id = order.uid().0.to_string();
            let copy_id = SharedString::from(format!("swap-order-{order_id}-copy"));
            rows.push(hash_row("Order ID", order_id, copy_id, "Copy order ID"));
            rows.push(fact_row("Settled by", settled_by_cow()));
        }
        if let SwapDelivery::External { receiver } = delivery {
            rows.push(self.receiver_row(receiver, cx));
        }
        if let Some((buy, minimum)) = minimum {
            rows.push(fact_row(
                "Receive at least",
                app_text(self.with_usd(self.token_amount(buy, minimum, cx), buy, minimum, cx)),
            ));
        }
        if let Some((label, at)) = started {
            rows.push(fact_row(label, app_text(local_date_time_label(at))));
        }
        (!rows.is_empty()).then(|| div().w_full().flex().flex_col().gap_2().children(rows))
    }

    /// A Public address swap's receiver, laid out like [`hash_row`]: the wallet's label for it
    /// when it has one, and its address, shortened, with a control that copies it in full.
    fn receiver_row(&self, receiver: Address, cx: &App) -> gpui::Div {
        let address = receiver.to_checksum(None);
        let copy_id = SharedString::from(format!("swap-receiver-{address}-copy"));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .debug_selector(|| "swap-detail-receiver".into())
            .child(app_muted_text("Receiver").flex_none())
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .children(
                        self.receiver_label(receiver, cx)
                            .map(|(label, _)| app_text(label).min_w_0().truncate()),
                    )
                    .child(
                        app_strong_text(short_receiver(receiver))
                            .flex_none()
                            .font_family(theme::APP_MONO_FONT_FAMILY),
                    )
                    .child(clipboard_with_toast(copy_id, address).tooltip("Copy receiver address")),
            )
    }

    /// A Bridge swap's facts: its receiver, destination and provider, then what its state
    /// needs, from the provider's deposit and the destination outcome to the settlement on this
    /// network. A deposit that needs attention shows its address in full. A private delivery
    /// has no receiver: its destination is the private balance on the destination network, and
    /// held proceeds name the destination stealth account.
    fn render_bridge_facts(
        &self,
        record: &ExecutorRecord,
        order: Option<&SwapOrderRecord>,
        delivery: BridgeDelivery,
        stage: SwapStage,
        started: Option<(&'static str, u64)>,
        cx: &App,
    ) -> gpui::Div {
        let network = network_name(delivery.destination_chain);
        let origin = network_name(self.session.chain_id);
        let provider = provider_name(delivery.provider);
        let private = delivery.is_private();
        let destination_amount = |amount| {
            self.network_token_amount(
                delivery.destination_chain,
                self.delivered_token(delivery, cx),
                amount,
                cx,
            )
        };
        let buy = order
            .and_then(|order| Some(record.swap()?.order_terms(order).buy_token()))
            .or_else(|| swap_tokens(record).map(|(_, buy)| buy));
        let observed = order.map(SwapOrderRecord::observations).unwrap_or_default();
        let terms = order.and_then(SwapOrderRecord::bridge);
        let (across, near) = match terms {
            Some(BridgeOrderTerms::Across(across)) => (Some(across), None),
            Some(BridgeOrderTerms::NearIntents(near)) => (None, Some(near)),
            None => (None, None),
        };
        let bounds = order
            .map(SwapOrderRecord::bounds)
            .or_else(|| record.swap_approval().map(|approval| &approval.bounds));
        // The private balance gets the minimum less the destination network's shield fee.
        let minimum = bounds.and_then(|bounds| {
            let minimum = bounds.destination_minimum?;
            Some(if private {
                private_delivery_credit(minimum, bounds)
            } else {
                minimum
            })
        });
        let receiver = (!private).then(|| self.receiver_row(delivery.receiver, cx));
        let destination = fact_row(
            "Destination",
            app_text(if private {
                format!("Private balance on {network}")
            } else {
                network.clone()
            }),
        );
        let provider_row = fact_row("Provider", app_text(provider));
        let deposit = observed
            .bridge_handoff
            .and_then(|handoff| handoff.deposit_id)
            .map(|id| {
                let id = id.to_string();
                let copy_id = SharedString::from(format!("swap-deposit-{id}-copy"));
                hash_row("Deposit ID", id, copy_id, "Copy Across deposit ID")
            })
            .or_else(|| {
                near.map(|near| {
                    address_row(
                        "Deposit address",
                        near.deposit_address,
                        "Copy deposit address",
                    )
                })
            });
        let settlement = observed
            .traded
            .and_then(|traded| traded.transaction_hash)
            .map(|hash| {
                let hash = hash.to_string();
                let copy_id = SharedString::from(format!("swap-settlement-{hash}-copy"));
                hash_row(
                    "Settlement",
                    hash,
                    copy_id,
                    "Copy settlement transaction hash",
                )
            });
        let started =
            started.map(|(label, at)| fact_row(label, app_text(local_date_time_label(at))));
        // What this session's last explicit check found in the stealth account.
        let checked = buy
            .zip(
                self.tracking
                    .get(&record.operation())
                    .and_then(|tracking| tracking.stealth_balance)
                    .map(|(balance, _)| balance),
            )
            .map(|(buy, balance)| {
                fact_row(
                    "In stealth account",
                    app_text(self.token_amount(buy, balance, cx)),
                )
            });
        let mut rows = Vec::new();
        let mut note = None;
        match stage {
            SwapStage::Order(SwapOrderState::Done) => {
                rows.extend(receiver);
                match observed.bridge_outcome {
                    Some(SwapBridgeOutcome::DeliveredVerified {
                        block,
                        output_amount,
                        shielded,
                        ..
                    }) => {
                        if private {
                            // What the destination stealth account shielded, less the fee.
                            let received =
                                bounds.filter(|_| shielded).map_or(output_amount, |bounds| {
                                    private_delivery_credit(output_amount, bounds)
                                });
                            rows.push(fact_row(
                                "Received",
                                amount_with_note(
                                    destination_amount(received),
                                    minimum.map_or_else(String::new, |minimum| {
                                        format!("(minimum {})", destination_amount(minimum))
                                    }),
                                ),
                            ));
                            rows.push(destination);
                        } else {
                            rows.push(fact_row(
                                "Delivered",
                                amount_with_note(
                                    destination_amount(output_amount),
                                    format!("on {network}"),
                                ),
                            ));
                        }
                        // A post-hook that reshields the surplus credits it privately.
                        rows.extend(buy.zip(observed.settlement_credit).map(|(buy, credit)| {
                            fact_row(
                                "Surplus",
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_wrap()
                                    .justify_end()
                                    .gap_1()
                                    .child(
                                        app_text(format!(
                                            "+{}",
                                            self.token_amount(buy, credit.private_amount, cx)
                                        ))
                                        .text_color(cx.theme().success),
                                    )
                                    .child(app_muted_text(format!("reshielded on {origin}"))),
                            )
                        }));
                        rows.push(provider_row);
                        // A fact, not a link: looking the block up is the user's choice.
                        rows.push(fact_row(
                            "Fill",
                            app_text(format!(
                                "{network} block {}",
                                railgun_ui::format_token_amount(U256::from(block.number), 0)
                            )),
                        ));
                        if !private {
                            rows.push(fact_row("Settled by", settled_by_cow()));
                        }
                    }
                    Some(SwapBridgeOutcome::DeliveredReported {
                        amount_out,
                        transaction_hash,
                    }) => {
                        rows.extend(amount_out.map(|amount| {
                            fact_row(
                                "Reported amount",
                                amount_with_note(
                                    destination_amount(amount),
                                    minimum.map_or_else(String::new, |minimum| {
                                        format!("(minimum {})", destination_amount(minimum))
                                    }),
                                ),
                            )
                        }));
                        // A copy button only, with no explorer link.
                        rows.extend(transaction_hash.map(|hash| {
                            let hash = hash.to_string();
                            let copy_id =
                                SharedString::from(format!("swap-destination-{hash}-copy"));
                            hash_row(
                                format!("{network} transaction"),
                                hash,
                                copy_id,
                                "Copy transaction hash",
                            )
                        }));
                        rows.push(provider_row);
                        rows.extend(deposit);
                        note = Some(format!(
                            "{provider} reports this delivery. The wallet doesn't check it on {network}, because looking the transaction up would tell the RPC provider which swap is yours."
                        ));
                    }
                    _ => rows.push(provider_row),
                }
                rows.extend(settlement);
            }
            SwapStage::Order(SwapOrderState::Refunding) => {
                rows.extend(receiver);
                rows.push(provider_row);
                rows.extend(deposit);
                rows.extend(across.map(|across| {
                    fact_row(
                        "Deposit expired",
                        app_text(local_date_time_label(u64::from(across.fill_deadline))),
                    )
                }));
                rows.extend(checked);
                note = Some(
                    "Check status confirms the refund reached the stealth account. Then Recover… shields it to your private balance for the shield fee and a broadcaster fee."
                        .to_owned(),
                );
            }
            SwapStage::Order(SwapOrderState::NeedsAttention) => {
                rows.extend(near.map(|near| deposit_address_box(near.deposit_address)));
                rows.extend(
                    buy.zip(order.and_then(bridge_sent_amount))
                        .map(|(buy, sent)| {
                            fact_row("Amount sent", app_text(self.token_amount(buy, sent, cx)))
                        }),
                );
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(settlement);
                note = Some(format!(
                    "Give {provider} support the deposit address. If 1Click refunds it, the {} goes to the stealth account on {origin}, and Check status finds it for recovery.",
                    buy.map_or_else(|| "deposit".to_owned(), |buy| self.token_symbol(buy, cx))
                ));
            }
            // The Across post-hook didn't run, so there's no deposit.
            SwapStage::Order(SwapOrderState::NotDelivered) => {
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(settlement);
                rows.extend(checked);
            }
            // The fill completed without its shield: the destination stealth account, the
            // fill, and what this session's last explicit check found in the account.
            SwapStage::Order(SwapOrderState::HeldOnDestination) => {
                rows.push(fact_row(
                    format!("Account on {network}"),
                    stealth_account(
                        "swap-held-account",
                        SwapStepAccount {
                            index: self
                                .destination_account(record, delivery)
                                .map(ExecutorRecord::index),
                            address: delivery.receiver,
                        },
                    ),
                ));
                rows.push(provider_row);
                if let Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) =
                    observed.bridge_outcome
                {
                    rows.push(fact_row(
                        "Fill",
                        app_text(format!(
                            "{network} block {}",
                            railgun_ui::format_token_amount(U256::from(block.number), 0)
                        )),
                    ));
                }
                rows.extend(
                    self.tracking
                        .get(&record.operation())
                        .and_then(|tracking| tracking.destination_balance)
                        .map(|(balance, _)| {
                            fact_row("In stealth account", app_text(destination_amount(balance)))
                        }),
                );
                note = Some(format!(
                    "Recover on {network}… shields it to your private balance on {network} for the shield fee and a broadcaster fee, paid from your private balance there."
                ));
            }
            SwapStage::Order(SwapOrderState::Traded | SwapOrderState::Bridging) => {
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                rows.extend(deposit);
                rows.extend(settlement);
                rows.extend(started);
            }
            // Before the trade: the order, and what the receiver gets on the destination network.
            _ => {
                if let Some(order) = order {
                    let order_id = order.uid().0.to_string();
                    let copy_id = SharedString::from(format!("swap-order-{order_id}-copy"));
                    rows.push(hash_row("Order ID", order_id, copy_id, "Copy order ID"));
                    rows.push(fact_row("Settled by", settled_by_cow()));
                }
                rows.extend(receiver);
                rows.extend([destination, provider_row]);
                // Across delivers its exact output; NEAR Intents at least its minimum.
                rows.extend(
                    minimum
                        .filter(|_| swap_order_group(stage, false, false) == SwapOrderGroup::Open)
                        .map(|minimum| {
                            let amount = destination_amount(minimum);
                            fact_row(
                                format!("Receive on {network}"),
                                app_text(match delivery.provider {
                                    BridgeProvider::Across => amount,
                                    BridgeProvider::NearIntents => format!("at least {amount}"),
                                }),
                            )
                        }),
                );
                rows.extend(started);
            }
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "swap-bridge-facts".into())
            .child(div().w_full().flex().flex_col().gap_2().children(rows))
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
    }

    /// What a traded order sold and delivered, from its approved bounds and canonical
    /// observations: the amounts, the price, the result against the minimum and the fees, then
    /// the order's details, collapsed. Rows without recorded data are left out. A Bridge
    /// swap's outcome is on its destination network, which [`Self::render_bridge_facts`] shows.
    fn render_outcome(
        &self,
        record: &ExecutorRecord,
        order: &SwapOrderRecord,
        stage: SwapStage,
        started: Option<(&'static str, u64)>,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if matches!(order.delivery(), SwapDelivery::Bridge(_)) {
            return None;
        }
        let SwapStage::Order(
            state @ (SwapOrderState::Traded | SwapOrderState::Done | SwapOrderState::NotDelivered),
        ) = stage
        else {
            return None;
        };
        let swap = record.swap()?;
        let outcome = swap_outcome(order.bounds(), &order.observations(), order.delivery())?;
        let terms = swap.order_terms(order);
        let (sell, buy) = (terms.sell_token(), terms.buy_token());
        let amount =
            |token, value| self.with_usd(self.token_amount(token, value, cx), token, value, cx);
        let (fees_open, details_open) = self.dialog.as_ref().map_or((false, false), |dialog| {
            (dialog.outcome_fees_open, dialog.outcome_details_open)
        });

        let received = outcome
            .received_privately
            .map(|(private, _)| private)
            .or_else(|| outcome.trade.map(|trade| trade.buy_amount));
        let destination = self.receiver_name(order.delivery(), cx).map_or_else(
            || {
                match state {
                    SwapOrderState::Done => "in your private balance",
                    SwapOrderState::NotDelivered => "in the stealth account",
                    _ => "arriving",
                }
                .to_owned()
            },
            // The trade paid the receiver in the same settlement.
            |receiver| format!("delivered to {receiver}"),
        );
        let with_usd_prefix = |usd: Option<String>, note: &str| {
            usd.map_or_else(|| note.to_owned(), |usd| format!("{usd} {note}"))
        };
        let amounts = div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .gap_3()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
            .child(outcome_side(
                self.token_icon(sell, cx),
                self.token_amount(sell, outcome.spent, cx),
                with_usd_prefix(self.usd_value(sell, outcome.spent, cx), "spent privately"),
                false,
            ))
            .child(
                app_muted_text("→")
                    .flex_none()
                    .text_size(theme::BALANCE_TEXT_SIZE),
            )
            .child(outcome_side(
                self.token_icon(buy, cx),
                received.map_or_else(
                    || format!("at least {}", self.token_amount(buy, outcome.minimum, cx)),
                    |received| self.token_amount(buy, received, cx),
                ),
                with_usd_prefix(
                    received.and_then(|received| self.usd_value(buy, received, cx)),
                    destination.as_str(),
                ),
                true,
            ));

        let mut rows = Vec::new();
        if let SwapDelivery::External { receiver } = order.delivery() {
            rows.push(self.receiver_row(receiver, cx).into_any_element());
        }
        let limit = self.pair_rate_label(sell, buy, outcome.limit_sell, outcome.minimum, cx);
        let (price, limit) = match outcome.trade.and_then(|trade| {
            self.pair_rate_label(sell, buy, trade.sell_amount, trade.buy_amount, cx)
        }) {
            Some(executed) => (Some(("Price", executed)), limit),
            None => (limit.map(|limit| ("Limit price", limit)), None),
        };
        rows.extend(price.map(|(label, price)| outcome_row(label, price, None).into_any_element()));
        // What was delivered against the approved minimum, then the fee the orderbook charged
        // beside the settlement transaction's gas cost, each once recorded.
        rows.extend(outcome.received.map(|received| {
            fact_row(
                "Received",
                div()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_1()
                    .child(app_text(amount(buy, received)))
                    .children(outcome.above_minimum.map(|above| {
                        app_text(format!(
                            "{} above your minimum",
                            self.bare_amount(buy, above, cx)
                        ))
                        .text_color(cx.theme().success)
                    })),
            )
            .debug_selector(|| "swap-outcome-received".into())
            .into_any_element()
        }));
        rows.push(
            self.minimum_row(buy, outcome.private_minimum, order, cx)
                .into_any_element(),
        );
        rows.extend(outcome.gas.map(|gas| {
            // The executed fee is everything the order was charged, not its gas alone, so the
            // two figures aren't compared.
            fact_row(
                "CoW fee",
                div()
                    .id("swap-outcome-gas-value")
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_1()
                    .tooltip(|window, cx| {
                        Tooltip::new(
                            "What CoW charged this order, network and protocol fees together, beside what the settlement's gas cost",
                        )
                        .build(window, cx)
                    })
                    .child(app_text(format!(
                        "{} charged",
                        self.money(gas.fee_token, gas.fee, cx)
                    )))
                    .child(app_muted_text(format!(
                        "· settlement gas {}",
                        self.money(Address::ZERO, gas.settlement_cost, cx)
                    ))),
            )
            .debug_selector(|| "swap-outcome-gas".into())
            .into_any_element()
        }));
        let unshield_fee = Some(outcome.unshield_fee).filter(|fee| !fee.is_zero());
        let shield_fee = outcome
            .received_privately
            .and_then(|(_, fee)| fee)
            .filter(|fee| !fee.is_zero());
        let order_fee = outcome
            .trade
            .map(|trade| trade.fee_amount)
            .filter(|fee| !fee.is_zero());
        let railgun_fees = [
            unshield_fee.map(|fee| (sell, fee)),
            shield_fee.map(|fee| (buy, fee)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !railgun_fees.is_empty() || order_fee.is_some() {
            // Two tokens, so one USD total, or none unless every fee has a rate.
            let usd = railgun_fees
                .iter()
                .try_fold(U256::ZERO, |total, &(token, fee)| {
                    Some(total.saturating_add(self.usd_micro_value(token, fee, cx)?))
                });
            let summary = (!railgun_fees.is_empty()).then(|| {
                usd.map_or_else(
                    || {
                        railgun_fees
                            .iter()
                            .map(|&(token, fee)| self.token_amount(token, fee, cx))
                            .collect::<Vec<_>>()
                            .join(" + ")
                    },
                    |usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)),
                )
            });
            let fees = outcome_disclosure(
                "swap-outcome-fees",
                "Fees",
                summary.map(app_text),
                fees_open,
                cx.listener(|this, _, _, cx| {
                    if let Some(dialog) = this.dialog.as_mut() {
                        dialog.outcome_fees_open = !dialog.outcome_fees_open;
                        cx.notify();
                    }
                }),
            );
            let fees = if fees_open {
                let mut fee_rows = Vec::new();
                fee_rows.extend(
                    unshield_fee.map(|fee| outcome_row("Unshield", amount(sell, fee), None)),
                );
                fee_rows
                    .extend(order_fee.map(|fee| outcome_row("Order fee", amount(sell, fee), None)));
                fee_rows
                    .extend(shield_fee.map(|fee| outcome_row("Shield", amount(buy, fee), None)));
                fee_rows.push(
                    app_muted_text("CoW's network and protocol fees are included in the price.")
                        .text_xs()
                        .whitespace_normal(),
                );
                fees.content(outcome_nested(fee_rows))
            } else {
                fees
            };
            rows.push(fees.into_any_element());
        }

        let details = outcome_disclosure(
            "swap-outcome-details",
            "Order details",
            started
                .filter(|&(label, _)| !details_open && label == "Started")
                .map(|(_, at)| app_muted_text(local_date_time_label(at))),
            details_open,
            cx.listener(|this, _, _, cx| {
                if let Some(dialog) = this.dialog.as_mut() {
                    dialog.outcome_details_open = !dialog.outcome_details_open;
                    cx.notify();
                }
            }),
        )
        .pt_3()
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE));
        let details = if details_open {
            let mut detail_rows = Vec::new();
            detail_rows.extend(
                started.map(|(label, at)| fact_row(label, app_text(local_date_time_label(at)))),
            );
            if let Some(trade) = outcome.trade {
                detail_rows.push(outcome_row(
                    "Sold to CoW",
                    amount(sell, trade.sell_amount),
                    None,
                ));
                detail_rows.push(outcome_row(
                    "Received from CoW",
                    amount(buy, trade.buy_amount),
                    None,
                ));
            }
            detail_rows.extend(limit.map(|limit| outcome_row("Limit price", limit, None)));
            let order_id = order.uid().0.to_string();
            let copy_id = SharedString::from(format!("swap-order-{order_id}-copy"));
            detail_rows.push(hash_row("Order ID", order_id, copy_id, "Copy order ID"));
            detail_rows.push(fact_row("Settled by", settled_by_cow()));
            detail_rows.extend(outcome.settlement.map(|hash| {
                let hash = hash.to_string();
                let copy_id = SharedString::from(format!("swap-settlement-{hash}-copy"));
                hash_row(
                    "Settlement",
                    hash,
                    copy_id,
                    "Copy settlement transaction hash",
                )
            }));
            details.content(outcome_nested(detail_rows))
        } else {
            details
        };
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(amounts)
                .child(div().w_full().flex().flex_col().gap_2().children(rows))
                .child(details)
                .debug_selector(|| "swap-outcome".into()),
        )
    }

    /// The approved minimum of `buy` and, when recorded, the gas share it was set with.
    fn minimum_row(
        &self,
        buy: Address,
        minimum: U256,
        order: &SwapOrderRecord,
        cx: &App,
    ) -> gpui::Div {
        fact_row(
            "Minimum",
            amount_with_note(
                self.with_usd(self.token_amount(buy, minimum, cx), buy, minimum, cx),
                order
                    .bounds()
                    .gas_share_bps
                    .map(gas_share_name)
                    .unwrap_or_default(),
            ),
        )
        .debug_selector(|| "swap-outcome-minimum".into())
    }

    /// An order that expired unfilled, as a normal outcome: its minimum, the gas it allowed
    /// against the estimate, when it expired and the gas price when it was signed, then where
    /// the inputs are. Rows a record from before gas shares lacks are left out. A Bridge
    /// swap's facts explain its own expiry.
    fn render_not_filled(
        &self,
        record: &ExecutorRecord,
        order: &SwapOrderRecord,
        stage: SwapStage,
        cx: &App,
    ) -> Option<gpui::Div> {
        if stage != SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired))
            || matches!(order.delivery(), SwapDelivery::Bridge(_))
        {
            return None;
        }
        let terms = record.swap()?.order_terms(order);
        let (sell, buy) = (terms.sell_token(), terms.buy_token());
        let bounds = order.bounds();
        let mut rows = vec![self.minimum_row(buy, bounds.private_minimum, order, cx)];
        if let (Some(allowance), Some(estimate)) = (bounds.gas_allowance, bounds.gas_estimate) {
            rows.push(
                fact_row(
                    "Gas you allowed",
                    app_text(format!(
                        "up to {} of ≈ {}",
                        self.money(buy, allowance, cx),
                        self.money(buy, estimate, cx)
                    )),
                )
                .debug_selector(|| "swap-not-filled-gas".into()),
            );
        }
        rows.push(fact_row(
            "Expired",
            app_text(local_date_time_label(u64::from(order.valid_to()))),
        ));
        rows.extend(bounds.gas_price_wei.map(|price| {
            fact_row(
                "Gas price at signing",
                app_text(format!("{} gwei", format_gwei(price))),
            )
            .debug_selector(|| "swap-not-filled-gas-price".into())
        }));
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().w_full().flex().flex_col().gap_2().children(rows))
                .child(
                    app_muted_text(format!(
                        "Nothing was unshielded. Your {} is still in your private balance. The setup fee isn't refunded.",
                        self.token_amount(sell, bounds.spend_amount(), cx)
                    ))
                    .whitespace_normal(),
                )
                .debug_selector(|| "swap-not-filled".into()),
        )
    }

    fn render_progress_actions(
        &self,
        operation: ExecutorOperationId,
        stage: SwapStage,
        actions: super::model::SwapActions,
        retry_destination: Option<u64>,
        job: Option<SwapJobKind>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let busy = self.busy();
        let record = self.record(operation);
        let stopped = record.is_some_and(ExecutorRecord::is_swap_setup_stopped);
        let hidden =
            record.is_some_and(|record| record.is_hidden() && self.pending_order(record).is_none());
        let setting_up =
            !stopped && matches!(stage, SwapStage::SetupSubmitting | SwapStage::SetupPending);
        let setup_job = self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
        let stop = setting_up.then(|| {
            app_button("swap-progress-stop", "Stop and remove…")
                .debug_selector(|| "swap-progress-stop".into())
                .small()
                .flex_none()
                .disabled(busy && !setup_job)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_stop_setup(operation, window, cx);
                }))
        });
        let resubmit = self
            .record(operation)
            .and_then(|record| record.swap())
            .and_then(|swap| swap.orders().last())
            .filter(|order| order.submission_status() == SwapSubmissionStatus::Pending)
            .filter(|_| {
                matches!(
                    stage,
                    SwapStage::SubmissionPending
                        | SwapStage::Order(SwapOrderState::PreHookOnly { expired: false })
                )
            })
            .map(|order| {
                let available = order.submission().is_some()
                    && u64::from(order.valid_to()) > now_unix().saturating_add(60);
                app_button("swap-progress-resubmit", "Submit again")
                    .primary()
                    .small()
                    .disabled(busy || !available)
                    .when(!available, |button| {
                        button.tooltip("Wait for final expiry, then retry with a new quote.")
                    })
                    .loading(job == Some(SwapJobKind::Order))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.resubmit_order(operation, window, cx);
                    }))
            });
        // A Bridge swap's check asks its provider again about a deposit that needs attention or
        // that Across refunds, and reads the stealth account for a refund or a deposit that
        // wasn't sent.
        let across_refund = stage == SwapStage::Order(SwapOrderState::Refunding)
            && record.is_some_and(refunds_across_deposit);
        let bridge_check = record.is_some_and(|record| {
            matches!(swap_delivery(record), SwapDelivery::Bridge(_))
                && matches!(
                    stage,
                    SwapStage::Order(
                        SwapOrderState::NeedsAttention
                            | SwapOrderState::Refunding
                            | SwapOrderState::NotDelivered
                    )
                )
        });
        // Held proceeds are looked for in the destination stealth account, on its network.
        let held = stage.is_held_on_destination();
        let check = record
            .and_then(|record| record.swap())
            .and_then(|swap| swap.orders().last())
            .filter(|order| {
                bridge_check
                    || held
                    || (stage.is_observed()
                        && (u64::from(order.valid_to()) < now_unix()
                            || stage == SwapStage::Order(SwapOrderState::Traded)))
            })
            .map(|_| {
                let checking = job == Some(SwapJobKind::Check);
                let label = if checking {
                    "Checking status"
                } else {
                    "Check status"
                };
                app_button("swap-progress-check", label)
                    .debug_selector(|| "swap-progress-check".into())
                    .outline()
                    .small()
                    .disabled(busy)
                    .when(checking, |button| button.icon(IconName::LoaderCircle))
                    .loading(checking)
                    .tooltip(match stage {
                        _ if held => "Checks the destination stealth account's balance with that network's RPC provider",
                        _ if !bridge_check => "Checks this stealth account's state with the RPC provider for retry or recovery",
                        SwapStage::Order(SwapOrderState::NeedsAttention) => {
                            "Asks the provider about the deposit, then checks this stealth account for a refund"
                        }
                        _ if across_refund => {
                            "Asks Across about the deposit and confirms its refund, then checks this stealth account's balance"
                        }
                        _ => "Checks this stealth account's balance with the RPC provider",
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if held {
                            this.check_destination_status(operation, window, cx);
                        } else if bridge_check {
                            this.check_bridge_status(operation, window, cx);
                        } else {
                            this.check_order_status(operation, window, cx);
                        }
                    }))
            });
        // A refund is recovered once an explicit check finds it in the stealth account. Kept
        // surplus can be there before an Across refund, so its refund must be verified first.
        let recover_blocked = if stage == SwapStage::Order(SwapOrderState::Refunding) {
            match self
                .tracking
                .get(&operation)
                .and_then(|tracking| tracking.stealth_balance)
            {
                _ if across_refund
                    && record
                        .and_then(|record| record.swap()?.orders().last())
                        .is_none_or(|order| order.observations().bridge_refund.is_none()) =>
                {
                    Some("Check status to confirm Across's refund to the stealth account.")
                }
                Some((balance, _)) if !balance.is_zero() => None,
                Some(_) => {
                    Some("The refund isn't in the stealth account yet. Check status again later.")
                }
                None => Some("Check status to find the refund in the stealth account."),
            }
        } else {
            None
        };
        let recover_is_next = stage.needs_recovery();
        let cancel = actions.cancel.map(|availability| {
            app_button("swap-progress-cancel-order", "Cancel order…")
                .danger()
                .small()
                .flex_none()
                .loading(job == Some(SwapJobKind::CancelQuote))
                .disabled(busy || availability.is_err())
                .when_some(availability.err(), gpui_component::button::Button::tooltip)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.request_cancel_quote(operation, window, cx);
                }))
        });
        let resume = actions.resume.map(|availability| {
            app_button(
                "swap-progress-continue",
                match stage {
                    SwapStage::SetupRetired => "New swap…",
                    SwapStage::Ready => "Review…",
                    SwapStage::Approved => "Place order…",
                    SwapStage::SetupPending => "Retry setup…",
                    _ => "Continue…",
                },
            )
            .primary()
            .small()
            .flex_none()
            .debug_selector(|| "swap-progress-continue".into())
            .loading(stage == SwapStage::Approved && job.is_some())
            .disabled((busy && !setup_job) || availability.is_err())
            .when_some(availability.err(), gpui_component::button::Button::tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                if this
                    .record(operation)
                    .is_none_or(|record| this.setup_retry_problem(record).is_some())
                {
                    return;
                }
                if stage == SwapStage::Approved {
                    this.place_approved_order(operation, window, cx);
                } else {
                    this.stop_setup_job(operation);
                    this.open_existing_form(operation, window, cx);
                }
            }))
        });
        let expired =
            stage == SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired));
        let retry = actions.retry.map(|availability| {
            app_button(
                "swap-progress-retry",
                if expired { "Swap again…" } else { "Retry…" },
            )
            .debug_selector(|| "swap-progress-retry".into())
            .when(availability.is_ok(), ButtonVariants::primary)
            .small()
            .flex_none()
            .disabled(busy || availability.is_err())
            .when_some(availability.err(), gpui_component::button::Button::tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_existing_form(operation, window, cx);
            }))
        });
        let recover = actions.recover.then(|| {
            app_button("swap-progress-recover", "Recover…")
                .debug_selector(|| "swap-progress-recover".into())
                .when(recover_is_next, ButtonVariants::primary)
                .small()
                .flex_none()
                .disabled(busy || recover_blocked.is_some())
                .tooltip(recover_blocked.unwrap_or("Opens this swap's stealth account recovery"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.recover(operation, window, cx);
                }))
        });
        // Recovery of held proceeds is the destination stealth account's, on its own network,
        // once that network's records tell the account.
        let recover_on_destination = record
            .filter(|_| actions.recover_on_destination)
            .and_then(|record| Some((record, swap_private_delivery(record)?)))
            .map(|(record, delivery)| {
                let network = network_name(delivery.destination_chain);
                let loaded = self.destination_account(record, delivery).is_some();
                app_button(
                    "swap-progress-recover-destination",
                    format!("Recover on {network}…"),
                )
                .debug_selector(|| "swap-progress-recover-destination".into())
                .small()
                .flex_none()
                .disabled(busy || !loaded)
                .tooltip(if loaded {
                    format!("Switches to {network} and opens the stealth account's recovery there")
                } else {
                    format!("Available once {network} has loaded")
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.recover_on_destination(operation, window, cx);
                }))
            });
        let retry_setup = retry_destination.map(|chain_id| {
            app_button(
                "swap-progress-retry-destination",
                format!("Retry setup on {}…", network_name(chain_id)),
            )
            .debug_selector(|| "swap-progress-retry-destination".into())
            .primary()
            .small()
            .flex_none()
            .loading(job == Some(SwapJobKind::SetupQuote))
            .disabled(busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.retry_destination_setup(operation, window, cx);
            }))
        });
        // A removed swap stays in My orders, so its detail opens without this action.
        let remove = (actions.dismiss && !hidden).then(|| {
            app_button("swap-progress-remove", "Remove from Private tab")
                .debug_selector(|| "swap-progress-remove".into())
                .small()
                .flex_none()
                .disabled(busy)
                .tooltip(
                    "Hides this card. Tracking and reserved funds are unchanged. The swap stays in My orders, and its account stays in Stealth accounts.",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_from_private_tab(operation, cx);
                }))
        });
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .children(cancel)
            .children(stop)
            .children(remove)
            .child(div().flex_1())
            .child(
                app_button("swap-progress-close", "Close")
                    .small()
                    .flex_none()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_swap_dialog(window, cx);
                    })),
            )
            .children(resubmit)
            .children(resume)
            .children(retry_setup)
            .children(retry)
            .children(check)
            .children(recover)
            .children(recover_on_destination)
    }

    /// Stop local delivery and automatic continuation. Already issued payloads remain
    /// recorded and observed because a broadcaster can still submit them.
    fn stop_setup_job(&mut self, operation: ExecutorOperationId) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup)
        {
            if let Some(job) = self.job.take() {
                job.abort.abort();
            }
            self.job_revision = self.job_revision.wrapping_add(1);
        }
        if let Some(tracking) = self.tracking.get_mut(&operation) {
            tracking.auto_place = false;
            tracking.setup_stage = None;
            tracking.setup_watch = None;
        }
    }

    fn confirm_stop_setup(
        &self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .record(operation)
            .is_none_or(|record| record.swap().is_some())
        {
            return;
        }
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(app_strong_text("Stop and remove this swap?"))
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text("Keep waiting")
                        .ok_text("Stop and remove")
                        .ok_variant(ButtonVariant::Danger),
                )
                .confirm()
                .child(app_text("No order will be placed. A setup already sent to a broadcaster may still confirm and charge its fee. Its private fee inputs stay reserved until resolved. The account remains in Stealth accounts for recovery.").whitespace_normal())
                .on_ok(move |_, _, cx| {
                    view.update(cx, |view, cx| view.stop_setup(operation, cx))
                        .unwrap_or(false)
                })
        });
    }

    /// The detail stays open and shows the stopped setup, which remains in My orders. A
    /// private Bridge swap's destination stealth account is stopped with it when its network is
    /// loaded. Otherwise that network's next load stops it.
    fn stop_setup(&mut self, operation: ExecutorOperationId, cx: &mut Context<'_, Self>) -> bool {
        if !self.session_is_current(cx)
            || self
                .record(operation)
                .is_none_or(|record| record.swap().is_some())
        {
            return false;
        }
        let destination = self.record(operation).and_then(|record| {
            let delivery = swap_private_delivery(record)?;
            let (_, owner) = self.destination_owner(delivery.destination_chain, cx)?;
            Some((owner, record.destination_operation()?))
        });
        if let Err(error) = self.owner.stop_swap_setup(operation) {
            self.fail(operation, error.to_string());
            cx.notify();
            return false;
        }
        if let Some((owner, destination_operation)) = destination {
            // A failed write is repeated when the destination network reconciles its accounts.
            let _ = owner.stop_swap_setup(destination_operation);
        }
        self.stop_setup_job(operation);
        self.form = None;
        self.reload_records();
        cx.notify();
        true
    }

    /// Account-specific reconciliation is an explicit action, separate from private
    /// whole-block confirmation. An orderbook expiry report alone cannot free inputs.
    fn check_order_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(confirmed) = self.confirmed_block(cx) else {
            self.fail(
                operation,
                "Status is unavailable until the network has synced. Try again once sync resumes."
                    .into(),
            );
            cx.notify();
            return;
        };
        let Some(mut cursor) = self
            .record(operation)
            .and_then(super::model::swap_history_start)
        else {
            self.fail(
                operation,
                "This order's saved history is unavailable, so its status could not be checked."
                    .into(),
            );
            cx.notify();
            return;
        };
        let owner = Arc::clone(&self.owner);
        let client = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.orderbook.clone());
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                let fee_uid = loop {
                    let range = super::model::swap_observation_range(cursor, confirmed);
                    cursor = range.end;
                    let report = owner.observe_swap(operation, range).await?;
                    if cursor > confirmed
                        || !super::model::swap_stage(report.record(), None, false).is_observed()
                    {
                        break report
                            .record()
                            .swap()
                            .and_then(|swap| swap.orders().last())
                            .filter(|order| needs_executed_fee(order))
                            .map(SwapOrderRecord::uid);
                    }
                };
                // A recorded trade still without its fee asks the orderbook once more on the
                // swap's own route. A failed read leaves the CoW fee row out.
                let Some(uid) = fee_uid else {
                    return Ok(None);
                };
                let client = match client {
                    Some(client) => Some(client),
                    None => owner.swap_orderbook_client().await.ok(),
                };
                if let Some(client) = &client {
                    let _ = Box::pin(owner.observe_swap_executed_fee(operation, uid, client)).await;
                }
                Ok(Some((uid, client)))
            },
            move |this, fee, _, _| {
                let uid = this
                    .record(operation)
                    .and_then(|record| record.swap())
                    .and_then(|swap| swap.orders().last())
                    .map(SwapOrderRecord::uid);
                let tracking = this.tracking.entry(operation).or_default();
                tracking.error = None;
                tracking.status_checked = uid.map(|uid| (uid, now_unix()));
                if let Some((uid, client)) = fee {
                    tracking.fee_asked.insert(uid);
                    if tracking.orderbook.is_none() {
                        tracking.orderbook = client;
                    }
                }
            },
            window,
            cx,
        );
    }

    /// A Bridge swap's explicit check. A deposit that needs attention is asked about again on
    /// the swap's own route, with the destination network's settings; a refund, then or
    /// already recorded, and a deposit that wasn't sent are looked for in the stealth account.
    fn check_bridge_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some(order) = record.swap().and_then(|swap| swap.orders().last()) else {
            return;
        };
        let SwapDelivery::Bridge(delivery) = order.delivery() else {
            return;
        };
        let stage = self.stage(record);
        let across_refund =
            stage == SwapStage::Order(SwapOrderState::Refunding) && refunds_across_deposit(record);
        let uid = order.uid();
        let buy = record
            .swap()
            .map(|swap| swap.order_terms(order).buy_token());
        let destination = self.root.upgrade().and_then(|root| {
            root.read(cx)
                .effective_chain_configs
                .get(delivery.destination_chain)
                .filter(|chain| chain.enabled)
                .cloned()
        });
        let network = network_name(delivery.destination_chain);
        let tracking = self.tracking.get(&operation);
        let client = tracking.and_then(|tracking| tracking.orderbook.clone());
        let clients = tracking.and_then(|tracking| tracking.bridge_clients.clone());
        let owner = Arc::clone(&self.owner);
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                let asked = if stage == SwapStage::Order(SwapOrderState::NeedsAttention)
                    || across_refund
                {
                    let destination = destination.ok_or_else(|| {
                        eyre::eyre!("Turn on {network} in Settings to check this swap's delivery.")
                    });
                    let ask = async {
                        let destination = destination?;
                        let client = match client {
                            Some(client) => client,
                            None => owner.swap_orderbook_client().await?,
                        };
                        let clients = match clients {
                            Some(clients) => clients,
                            None => owner.swap_bridge_clients(&client)?,
                        };
                        let outcome = Box::pin(owner.check_swap_bridge(
                            operation,
                            uid,
                            &clients,
                            &destination,
                        ))
                        .await?;
                        Ok::<_, eyre::Report>((client, clients, outcome))
                    };
                    Some(ask.await)
                } else {
                    None
                };
                // The refund is on this network, so a failed provider or destination check
                // still looks for it in the stealth account.
                let (route, refunding, problem) = match asked {
                    Some(Ok((client, clients, outcome))) => (
                        Some((client, clients)),
                        outcome == Some(SwapBridgeOutcome::Refunding),
                        None,
                    ),
                    Some(Err(error)) if across_refund => (None, true, Some(format!("{error:#}"))),
                    Some(Err(error)) => return Err(error),
                    None => (
                        None,
                        stage == SwapStage::Order(SwapOrderState::Refunding),
                        None,
                    ),
                };
                let balance = match buy {
                    Some(buy)
                        if refunding || stage == SwapStage::Order(SwapOrderState::NotDelivered) =>
                    {
                        let asset = ExecutorAsset::Erc20(buy);
                        let inspection = owner.inspect_record(operation, &[asset]).await?;
                        inspection
                            .balances()
                            .get(&asset)
                            .copied()
                            .flatten()
                            .map(|balance| (balance, inspection.block()))
                    }
                    _ => None,
                };
                Ok((route, balance, problem))
            },
            move |this, (route, balance, problem), _, _| {
                let tracking = this.tracking.entry(operation).or_default();
                if let Some((client, clients)) = route {
                    if tracking.orderbook.is_none() {
                        tracking.orderbook = Some(client);
                    }
                    if tracking.bridge_clients.is_none() {
                        tracking.bridge_clients = Some(clients);
                    }
                }
                if balance.is_some() {
                    tracking.stealth_balance = balance;
                }
                tracking.error = None;
                if let Some(problem) = problem {
                    this.fail(operation, problem);
                }
            },
            window,
            cx,
        );
    }

    fn resubmit_order(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let client = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.orderbook.clone());
        let owner = Arc::clone(&self.owner);
        self.start_job(
            operation,
            SwapJobKind::Order,
            async move {
                let client = match client {
                    Some(client) => client,
                    None => owner.swap_orderbook_client().await?,
                };
                let outcome = owner.resubmit_swap_order(operation, &client).await?;
                Ok((client, outcome))
            },
            move |this, (client, outcome), window, cx| {
                this.tracking.entry(operation).or_default().orderbook = Some(client);
                this.finish_order(operation, outcome, window, cx);
            },
            window,
            cx,
        );
    }

    fn request_cancel_quote(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some((sell, _)) = self.record(operation).and_then(swap_tokens) else {
            return;
        };
        let (_, token, candidates) =
            self.setup_fee_route(self.session.chain_id, sell, None, false, false, cx);
        let candidate = token.and_then(|_| {
            let root = self.root.upgrade()?;
            let root = root.read(cx);
            wallet_ops::select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &PublicBroadcasterSelection::Random,
                root.public_broadcaster_fee_policy(false),
                &root.public_broadcaster_trust_filter(false),
            )
            .ok()
        });
        let Some(candidate) = candidate else {
            self.fail(
                operation,
                "No compatible broadcaster accepts a private fee token you hold. Wait for the order to expire at no cost, or try again later."
                    .into(),
            );
            cx.notify();
            return;
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::CancelQuote,
            async move {
                Box::pin(owner.estimate_swap_cancellation_fee(operation, &session, candidate)).await
            },
            move |this, estimate, window, cx| {
                this.cancel = Some(CancelQuote {
                    operation,
                    estimate,
                });
                this.open_cancel_confirmation(window, cx);
            },
            window,
            cx,
        );
    }

    fn open_cancel_confirmation(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(quote) = self.cancel.as_ref() else {
            return;
        };
        let fee = self.token_amount(
            quote.estimate.broadcaster().token,
            quote.estimate.fee_amount(),
            cx,
        );
        let wait = self
            .record(quote.operation)
            .and_then(swap_valid_to)
            .map_or_else(
                || "If you wait, the order expires at no cost.".to_owned(),
                |valid_to| {
                    format!(
                        "If you wait, the order expires at {} at no cost.",
                        local_time_label(valid_to)
                    )
                },
            );
        let body = SharedString::from(format!(
            "Cancelling now costs about {fee} through a broadcaster. {wait} Anyone can trigger the swap's unshield before expiry. If the order then doesn't fill, recovery costs an unshield fee and a shield fee."
        ));
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, window, _cx| {
            let confirm_view = view.clone();
            let cancel_view = view.clone();
            let width =
                (window.viewport_size().width * 0.92).min(rems(27.5).to_pixels(window.rem_size()));
            dialog
                .width(width)
                .max_h(dialog_max_height(window))
                .title(app_strong_text("Cancel the swap order now?"))
                .button_props(
                    DialogButtonProps::default()
                        .cancel_text("Keep waiting")
                        .cancel_variant(ButtonVariant::Secondary)
                        .ok_text("Cancel order")
                        .ok_variant(ButtonVariant::Danger),
                )
                .confirm()
                .child(app_text(body.clone()).whitespace_normal())
                .on_cancel(move |_, _, cx| {
                    let _ = cancel_view.update(cx, |view, cx| {
                        view.cancel = None;
                        cx.notify();
                    });
                    true
                })
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    let _ = confirm_view.update(cx, |view, cx| {
                        view.request_cancel_authorization(window, cx);
                    });
                    false
                })
        });
    }

    fn request_cancel_authorization(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(quote) = self.cancel.take() else {
            return;
        };
        let operation = quote.operation;
        let Some(waku) = self.broadcaster_network(cx) else {
            self.fail(
                operation,
                "Wait for the broadcaster network connection, then try again.".into(),
            );
            cx.notify();
            return;
        };
        let approval = CancelApproval {
            operation,
            candidate: quote.estimate.broadcaster().clone(),
            maximum_private_fee: quote.estimate.fee_amount(),
            waku,
        };
        let order = self
            .record(operation)
            .map_or_else(String::new, |record| self.labels(record, cx).pair);
        let fee = self.token_amount(approval.candidate.token, approval.maximum_private_fee, cx);
        let summary = SpendAuthorizationSummary::new(
            "Cancel swap order",
            "Pay a broadcaster privately to make this swap's open order unfillable.",
            vec![
                SpendAuthorizationSummaryRow::new("Order", order),
                SpendAuthorizationSummaryRow::new("Private fee", format!("Up to {fee}")),
                SpendAuthorizationSummaryRow::new(
                    "Broadcaster",
                    broadcaster_candidate_label(&approval.candidate),
                ),
            ],
        )
        .with_context(
            "The cancellation takes the pre-hook's nonce, so the swap's unshield can't run afterwards. Anyone can trigger the unshield first, in which case the swap continues and the cancellation doesn't apply. If the order then doesn't fill, recovery costs an unshield fee and a shield fee.",
        )
        .with_confirm_label("Authorize cancellation")
        .requiring_explicit_review();
        self.request_authorization(SwapAction::Cancel(Box::new(approval)), summary, window, cx);
    }

    pub(super) fn submit_cancellation(
        &mut self,
        approval: CancelApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let operation = approval.operation;
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::Cancel,
            async move {
                let prepared = Box::pin(owner.prepare_swap_cancellation(
                    operation,
                    approval.candidate,
                    approval.maximum_private_fee,
                    &authorization,
                ))
                .await?;
                let outcome = Box::pin(owner.submit_paid_recovery(ExecutorPaidRecoveryRequest {
                    recovery: Arc::new(prepared),
                    session,
                    authorization,
                    waku: approval.waku,
                    verify_proof: true,
                    progress_tx: None,
                    response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                    republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                }))
                .await?;
                Ok(outcome.result)
            },
            move |this, result, window, cx| {
                let problem = broadcaster_result_problem(&result, "cancellation");
                let tracking = this.tracking.entry(operation).or_default();
                tracking.cancelling = problem.is_none();
                tracking.error = problem;
                if !window.has_active_dialog(cx) {
                    this.show_detail(operation, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Recovery lives in Stealth accounts; open it there for this swap's account, on top of the
    /// swap dialog, which gets focus back when recovery closes or doesn't open.
    fn recover(
        &self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some((sell, buy)) = swap_tokens(record) else {
            return;
        };
        let token = swap_recovery_token(self.stage(record), swap_delivery(record), sell, buy);
        // Recovery offers what a balance check found, so this swap's own check goes with it.
        let checked = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.stealth_balance)
            .filter(|_| token == buy);
        let target = StealthAccountTarget::new(&self.session, operation);
        let return_focus = self.swap_dialog_focus();
        let _ = self.root.update(cx, |root, cx| {
            root.open_stealth_account_recovery(
                &target,
                ExecutorAsset::Erc20(token),
                checked,
                return_focus,
                window,
                cx,
            );
        });
    }

    /// Held proceeds are recovered in Stealth accounts on the destination network. Switching
    /// to that network closes this dialog and replaces this view, so the root does both after
    /// this update: it switches, then opens the destination stealth account's recovery with
    /// this session's last check of its balance.
    fn recover_on_destination(
        &self,
        operation: ExecutorOperationId,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some(delivery) = swap_private_delivery(record) else {
            return;
        };
        let Some(destination) = self.destination_account(record, delivery) else {
            return;
        };
        let destination = destination.operation();
        let checked = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.destination_balance);
        let root = self.root.clone();
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| {
                root.open_stealth_account_recovery_on(
                    delivery.destination_chain,
                    destination,
                    ExecutorAsset::Erc20(delivery.destination_token),
                    checked,
                    window,
                    cx,
                );
            });
        });
    }

    /// An explicit check of held proceeds, through the destination network's owner: it settles
    /// the destination stealth account's record from this swap's outcome, then reads the
    /// delivered token's balance there, which recovery opens with.
    fn check_destination_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(delivery) = self.record(operation).and_then(swap_private_delivery) else {
            return;
        };
        let chain_id = delivery.destination_chain;
        let destination = self
            .record(operation)
            .and_then(|record| self.destination_account(record, delivery))
            .map(ExecutorRecord::operation);
        let Some(((_, owner), destination)) = self.destination_owner(chain_id, cx).zip(destination)
        else {
            self.ensure_destination_load(chain_id, cx);
            self.reload_destinations(cx);
            self.fail(
                operation,
                format!(
                    "{} isn't loaded yet. Check again once it has synced.",
                    network_name(chain_id)
                ),
            );
            cx.notify();
            return;
        };
        let asset = ExecutorAsset::Erc20(delivery.destination_token);
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                owner.reconcile_swap_destinations()?;
                let inspection = owner.inspect_record(destination, &[asset]).await?;
                Ok(inspection
                    .balances()
                    .get(&asset)
                    .copied()
                    .flatten()
                    .map(|balance| (balance, inspection.block())))
            },
            move |this, balance, _, _| {
                let tracking = this.tracking.entry(operation).or_default();
                if balance.is_some() {
                    tracking.destination_balance = balance;
                }
                tracking.error = None;
            },
            window,
            cx,
        );
    }

    /// Removal hides the swap's account, which is persisted with the encrypted record. The swap
    /// stays in My orders, and an account that needs attention stays listed in Stealth accounts.
    fn remove_from_private_tab(
        &mut self,
        operation: ExecutorOperationId,
        cx: &mut Context<'_, Self>,
    ) {
        if let Err(error) = self.owner.set_hidden(operation, true) {
            self.fail(operation, error.to_string());
            cx.notify();
            return;
        }
        if let Some(tracking) = self.tracking.get_mut(&operation) {
            tracking.pending_order = None;
        }
        self.reload_records();
        cx.notify();
    }
}

/// Whether the record's latest order is an Across Bridge order, whose refund an explicit check
/// verifies on this network.
fn refunds_across_deposit(record: &ExecutorRecord) -> bool {
    record
        .swap()
        .and_then(|swap| swap.orders().last())
        .is_some_and(|order| matches!(order.bridge(), Some(BridgeOrderTerms::Across(_))))
}

/// A hash, shortened, with a control that copies it in full.
fn hash_row(
    label: impl Into<SharedString>,
    hash: String,
    copy_id: SharedString,
    tooltip: &'static str,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(app_strong_text(short_hash(&hash)).font_family(theme::APP_MONO_FONT_FAMILY))
                .child(clipboard_with_toast(copy_id, hash).tooltip(tooltip)),
        )
}

/// An address, shortened, with a control that copies it in full, laid out like [`hash_row`].
fn address_row(label: &'static str, address: Address, tooltip: &'static str) -> gpui::Div {
    let short = short_receiver(address);
    let address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("swap-address-{address}-copy"));
    fact_row(
        label,
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(app_strong_text(short).font_family(theme::APP_MONO_FONT_FAMILY))
            .child(clipboard_with_toast(copy_id, address).tooltip(tooltip)),
    )
}

/// A deposit address in full, so it can be read out or checked character by character. It
/// wraps inside its box, and its copy control stays at the trailing edge.
fn deposit_address_box(address: Address) -> gpui::Div {
    let address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("swap-deposit-address-{address}-copy"));
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Deposit address"))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
                .debug_selector(|| "swap-detail-deposit-address".into())
                .child(
                    app_strong_text(address.clone())
                        .flex_1()
                        .min_w_0()
                        .font_family(theme::APP_MONO_FONT_FAMILY)
                        .whitespace_normal(),
                )
                .child(
                    div().flex_none().child(
                        clipboard_with_toast(copy_id, address).tooltip("Copy deposit address"),
                    ),
                ),
        )
}

/// An amount and a muted note after it, wrapping to the trailing edge when narrow.
fn amount_with_note(amount: String, note: String) -> gpui::Div {
    div()
        .min_w_0()
        .flex()
        .flex_wrap()
        .justify_end()
        .gap_1()
        .child(app_text(amount))
        .when(!note.is_empty(), |value| value.child(app_muted_text(note)))
}

/// One fact, laid out like [`hash_row`].
fn fact_row(label: impl Into<SharedString>, value: impl gpui::IntoElement) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(value)
}

/// Earlier attempts of this pair on the swap's stealth account that ended without a fill.
fn earlier_attempts_note(record: &ExecutorRecord, range: std::ops::Range<usize>) -> Option<String> {
    let orders = record.swap()?.orders().get(range)?;
    // Only a swap's last order can trade, so its earlier orders all ended without a fill.
    let (_, earlier) = orders.split_last()?;
    let expired = |order: &SwapOrderRecord| {
        let observed = order.observations();
        observed.expired.is_some()
            || observed
                .pre_hook_dead
                .is_some_and(|death| death.cause == SwapPreHookDeathCause::Expired)
    };
    match earlier {
        [] => None,
        [order] if expired(order) => Some(format!(
            "1 earlier attempt expired at {} without a fill.",
            local_time_label(u64::from(order.valid_to()))
        )),
        [_] => Some("1 earlier attempt ended without a fill.".into()),
        orders if orders.iter().all(expired) => Some(format!(
            "{} earlier attempts expired without a fill.",
            orders.len()
        )),
        orders => Some(format!(
            "{} earlier attempts ended without a fill.",
            orders.len()
        )),
    }
}

/// A swap step and its sub-steps for the shared stepper. A sub-step names its network and its
/// stealth account, with a control that copies the account's address, and ends with its block
/// or what it waits for.
fn progress_group(step: &SwapStep, detail: String, id: String) -> SubmissionProgressGroup {
    let substeps = step
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            let key = child.account.map_or_else(
                || index.to_string(),
                |account| account.address.to_checksum(None),
            );
            SubmissionProgressSubstep {
                id: SharedString::from(format!("{id}-{key}")),
                label: child.label.clone(),
                status: child.status,
                content: child.account.map(|account| {
                    stealth_account("swap-step-account", account).into_any_element()
                }),
                outcome: child.detail.clone(),
            }
        })
        .collect();
    SubmissionProgressGroup {
        step: progress_step(step, detail, id),
        substeps,
    }
}

/// A stealth account's number and short address, with a control that copies the address in
/// full. `id` tells the account's places on one detail apart.
fn stealth_account(id: &'static str, account: SwapStepAccount) -> gpui::Div {
    let short = short_receiver(account.address);
    let address = account.address.to_checksum(None);
    let copy_id = SharedString::from(format!("{id}-{address}-copy"));
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .child(
            app_muted_text(
                account
                    .index
                    .map_or_else(|| short.clone(), |index| format!("#{index} · {short}")),
            )
            .font_family(theme::APP_MONO_FONT_FAMILY),
        )
        .child(clipboard_with_toast(copy_id, address).tooltip("Copy stealth account address"))
}

/// A swap step for the shared stepper, whose body hides a detail equal to its label.
fn progress_step(step: &SwapStep, detail: String, error_copy_id: String) -> SubmissionProgressStep {
    SubmissionProgressStep {
        detail: if detail.is_empty() {
            step.label.clone()
        } else {
            detail
        },
        label: step.label.clone(),
        status: step.status,
        error_copy_id: SharedString::from(error_copy_id),
        action: None,
    }
}

/// A completed swap's title says so, so its outcome replaces its steps. Steps that still wait
/// or need attention stay above the outcome.
fn shows_steps(stage: SwapStage, outcome: bool) -> bool {
    !outcome || stage != SwapStage::Order(SwapOrderState::Done)
}

/// One outcome value, laid out like [`hash_row`], with an optional note under the value.
fn outcome_row(label: &'static str, value: String, note: Option<String>) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_start()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .items_end()
                .child(app_text(value).whitespace_normal())
                .children(note.map(|note| app_muted_text(note).text_xs().whitespace_normal())),
        )
}

/// One side of a traded swap's amounts: the token's icon and amount, and a muted line under it.
/// The trailing side aligns to the end.
fn outcome_side(
    icon: Option<WalletIconSource>,
    amount: String,
    note: String,
    trailing: bool,
) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .when(trailing, gpui::Styled::items_end)
        .child(
            div()
                .max_w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .children(icon.map(|icon| img(icon).size(px(20.0)).rounded_full().flex_none()))
                .child(
                    app_strong_text(amount)
                        .min_w_0()
                        .text_size(theme::BALANCE_TEXT_SIZE)
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_normal()
                        .when(trailing, gpui::Styled::text_right),
                ),
        )
        .child(
            app_muted_text(note)
                .text_xs()
                .whitespace_normal()
                .when(trailing, gpui::Styled::text_right),
        )
}

/// A disclosure row of a traded swap's outcome: its label, an optional summary and a chevron.
/// The caller adds the content while it's open.
fn outcome_disclosure(
    id: &'static str,
    label: &'static str,
    summary: Option<gpui::Div>,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Collapsible {
    let toggle = format!(
        "{} {}",
        if open { "Hide" } else { "Show" },
        label.to_lowercase()
    );
    Collapsible::new()
        .open(open)
        .w_full()
        .min_w_0()
        .gap_2()
        .child(
            app_button_base(id)
                .ghost()
                .w_full()
                .min_w_0()
                .h_auto()
                .px_0()
                .py_0()
                .accessibility_label(toggle.clone())
                .tooltip(toggle)
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(app_muted_text(label).flex_1().min_w_0())
                        .children(summary.map(gpui::Styled::flex_none))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_MUTED)),
                        ),
                )
                .on_click(on_toggle),
        )
}

/// An open disclosure's rows, indented behind a rule.
fn outcome_nested(rows: Vec<gpui::Div>) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .pl_3()
        .border_l_2()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .children(rows)
}

const fn progress_note(stage: SwapStage) -> Option<&'static str> {
    match stage {
        SwapStage::SubmissionPending | SwapStage::SubmissionRejected => Some(
            "The signed order stays reserved until its expiry is final. A submission timeout does not prove that the orderbook rejected it.",
        ),
        SwapStage::SetupSubmitting
        | SwapStage::SetupPending
        | SwapStage::Order(
            SwapOrderState::Open
            | SwapOrderState::Traded
            | SwapOrderState::Bridging
            | SwapOrderState::PreHookOnly { expired: false },
        ) => Some(
            "You can close this. The swap keeps running and its status stays on the Private tab.",
        ),
        SwapStage::Order(SwapOrderState::AttemptEnded(_)) => Some(
            "Retrying reuses this stealth account, so there's no second setup cost. You review a new quote first.",
        ),
        SwapStage::Order(
            SwapOrderState::PreHookOnly { expired: true } | SwapOrderState::NotDelivered,
        ) => Some(
            "Recovering shields the funds back to your private balance. It costs the shield fee and a broadcaster fee.",
        ),
        SwapStage::Ready => {
            Some("The stealth account is set up. Review a quote to place the order.")
        }
        SwapStage::Approved => Some(
            "The stealth account is set up. Placing the order checks the fee, the price and your minimum again. If one changed, you review the swap again.",
        ),
        SwapStage::SetupRetired => Some(
            "Start a new swap with a fresh stealth account. The old account remains available for recovery if it holds any funds.",
        ),
        SwapStage::SetupNotSent | SwapStage::SetupFailed => Some(
            "Continue to send the setup again with the same stealth account. Nothing was unshielded.",
        ),
        // A Bridge swap's facts explain its refund, what needs attention, or the recovery of
        // held proceeds on the destination network.
        SwapStage::Order(
            SwapOrderState::Done
            | SwapOrderState::Refunding
            | SwapOrderState::NeedsAttention
            | SwapOrderState::HeldOnDestination,
        )
        | SwapStage::Recovered => None,
    }
}
