//! Public-paid swaps are presented on the destination network that owns their durable use.

use alloy::primitives::{Address, U256};
use gpui::{App, Context, ParentElement as _, SharedString, Styled as _, div};

use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, alert::Alert, button::ButtonVariants as _,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_muted_text, app_strong_text, app_text};
use ui::theme;
use wallet_ops::vault::{
    BridgeProvider, ExecutorRecord, PublicSwapRecord, SwapBridgeOutcome, SwapUseRecord, SwapUseRole,
};

use super::form::network_name;
use super::model::{
    PublicCardLine, PublicSwapLabels, PublicSwapStage, SwapIdentity, SwapOrderGroup, SwapStage,
    private_delivery_credit, provider_name, public_swap_stage, public_swap_steps, swap_setup_block,
};
use super::progress::{
    address_row, amount_with_note, block_label, fact_row, hash_row, progress_group,
};
use super::{PrivateSwapsView, local_date_time_label, local_time_label, now_unix, short_receiver};
use crate::root::public_action::PublicActionStepStatus;
use crate::root::submission_progress::render_submission_progress_groups;

/// Background status checks that fail in a row before the detail view says so.
const PUBLIC_TRACKING_FAILURES_SHOWN: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicSwapAction {
    CheckStatus,
    CancelOrder,
    Withdraw,
    RetrySetup,
    Retry,
    Continue,
    CancelPreparation,
    RecoverDestination,
}

/// One row of the order detail's facts. A row whose value isn't recorded is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicSwapFact {
    Received,
    ReportedAmount,
    /// The Public account, with a control that copies its address while the swap is under way.
    PaidFrom {
        copy: bool,
    },
    Destination,
    Started,
    Provider,
    Fill,
    Deposit,
    DepositId,
    DepositExpired,
    CowProxy,
    ControlledBy,
    Settlement,
}

/// The facts each state shows, in order, as the mockup lists them.
pub(super) const fn public_swap_facts(stage: PublicSwapStage) -> &'static [PublicSwapFact] {
    use PublicSwapFact as Fact;
    match stage {
        PublicSwapStage::DeliveredVerified => &[
            Fact::Received,
            Fact::PaidFrom { copy: false },
            Fact::Destination,
            Fact::Provider,
            Fact::Fill,
            Fact::Settlement,
        ],
        PublicSwapStage::DeliveredReported => &[
            Fact::ReportedAmount,
            Fact::PaidFrom { copy: false },
            Fact::Destination,
            Fact::Provider,
            Fact::DepositId,
            Fact::Settlement,
        ],
        PublicSwapStage::Refunding | PublicSwapStage::Refunded => {
            &[Fact::Provider, Fact::DepositId, Fact::DepositExpired]
        }
        PublicSwapStage::HeldByProxy
        | PublicSwapStage::Withdrawing
        | PublicSwapStage::WithdrawnPendingCheck
        | PublicSwapStage::SwappedNotBridged => {
            &[Fact::CowProxy, Fact::ControlledBy, Fact::Settlement]
        }
        PublicSwapStage::HeldOnDestination | PublicSwapStage::Recovered => &[
            Fact::PaidFrom { copy: false },
            Fact::Destination,
            Fact::Provider,
            Fact::Fill,
            Fact::Settlement,
        ],
        PublicSwapStage::Bridging => &[
            Fact::PaidFrom { copy: true },
            Fact::Destination,
            Fact::Deposit,
        ],
        PublicSwapStage::NeedsAttention => &[
            Fact::PaidFrom { copy: true },
            Fact::Destination,
            Fact::Provider,
            Fact::Deposit,
            Fact::DepositId,
        ],
        PublicSwapStage::Preparing(_)
        | PublicSwapStage::Approving
        | PublicSwapStage::ApprovalFailed
        | PublicSwapStage::OrderOpen
        | PublicSwapStage::SubmissionPending
        | PublicSwapStage::SubmissionRejected
        | PublicSwapStage::Depositing
        | PublicSwapStage::DepositFailed
        | PublicSwapStage::Cancelled
        | PublicSwapStage::Expired
        | PublicSwapStage::PreparationCancelled => &[
            Fact::PaidFrom { copy: true },
            Fact::Destination,
            Fact::Started,
        ],
    }
}

impl PrivateSwapsView {
    pub(super) fn public_destination_balance(
        &self,
        record: &ExecutorRecord,
        claimed: &SwapUseRecord,
    ) -> Option<(U256, alloy::eips::BlockNumHash)> {
        let chain = self.public_record_chain(record)?;
        self.public_destination_balances
            .get(&(chain, record.operation(), claimed.id()))
            .copied()
            .filter(|(_, read_at)| {
                claimed.public_swap().is_some_and(|swap| {
                    matches!(
                        swap.observations().bridge_outcome,
                        Some(SwapBridgeOutcome::HeldOnDestination { block, .. })
                            if read_at.number >= block.number
                    )
                })
            })
    }

    pub(super) fn public_swap_record(
        &self,
        identity: SwapIdentity,
    ) -> Option<(&ExecutorRecord, &SwapUseRecord, &PublicSwapRecord)> {
        let record = self
            .public_records
            .iter()
            .map(|(_, record)| record)
            .find(|record| {
                record.operation() == identity.operation
                    && record.public_swap_use(identity.swap_use).is_some()
            })?;
        let (claimed, swap) = record.public_swap_use(identity.swap_use)?;
        Some((record, claimed, swap))
    }

    pub(super) fn shown_public_swaps(
        &self,
    ) -> impl Iterator<Item = (SwapIdentity, PublicSwapStage)> + '_ {
        self.public_records
            .iter()
            .filter(|(chain, _)| *chain == self.origin_chain_id)
            .flat_map(move |(_, record)| {
                record.swap_uses().iter().filter_map(move |claimed| {
                    let stage = public_swap_stage(
                        record,
                        claimed,
                        None,
                        now_unix(),
                        self.attribution(record),
                        self.public_destination_balance(record, claimed),
                    )?;
                    // Held funds stay visible for recovery. Reported delivery stays in history.
                    (stage.group() != SwapOrderGroup::Ended
                        || claimed.public_swap().is_some_and(|swap| {
                            !swap.is_finished(claimed.is_stopped(), now_unix())
                        }))
                    .then_some((
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: claimed.id(),
                        },
                        stage,
                    ))
                })
            })
    }

    pub(super) fn public_chain_amount(
        &self,
        chain: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        // A token the wallet has no metadata for reads as it does in a private swap.
        self.chain_token_metadata(chain, token, cx).map_or_else(
            || self.chain_amount(chain, token, amount, cx),
            |metadata| {
                format!(
                    "{} {}",
                    railgun_ui::format_token_amount(amount, metadata.decimals),
                    metadata.symbol
                )
            },
        )
    }

    /// The Public account's label, when the wallet has one for it.
    fn public_source_name(&self, source: Address, cx: &App) -> Option<String> {
        let root = self.root.upgrade()?;
        root.read(cx)
            .public_accounts
            .iter()
            .find(|account| account.address == source)
            .and_then(crate::root::public_account::public_account_display_label)
    }

    pub(super) fn public_labels(
        &self,
        record: &ExecutorRecord,
        claimed: &SwapUseRecord,
        cx: &App,
    ) -> Option<PublicSwapLabels> {
        let SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            swap,
            ..
        } = claimed.role()
        else {
            return None;
        };
        let destination = self.public_record_chain(record)?;
        let source_name = self
            .public_source_name(*source, cx)
            .unwrap_or_else(|| railgun_ui::short_address(source));
        let symbol = |chain, token| {
            self.chain_token_metadata(chain, token, cx).map_or_else(
                || railgun_ui::short_address(&token),
                |metadata| metadata.symbol,
            )
        };
        let observed = swap.observations();
        let bounds = &swap.approval().bounds;
        let deposited = observed.deposited.map(|deposit| {
            self.public_chain_amount(
                *origin_chain,
                swap.intent().bridged_token,
                deposit.input_amount,
                cx,
            )
        });
        let received = match observed.bridge_outcome {
            Some(SwapBridgeOutcome::DeliveredVerified { output_amount, .. }) => {
                Some(self.public_chain_amount(
                    destination,
                    *destination_token,
                    private_delivery_credit(output_amount, bounds),
                    cx,
                ))
            }
            Some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(amount),
                ..
            }) => Some(self.public_chain_amount(destination, *destination_token, amount, cx)),
            _ => None,
        };
        let held = match observed.bridge_outcome {
            Some(SwapBridgeOutcome::HeldOnDestination { .. }) => {
                super::model::public_held_remaining(
                    claimed,
                    self.public_destination_balance(record, claimed),
                )
                .map(|amount| self.public_chain_amount(destination, *destination_token, amount, cx))
            }
            _ => observed.held_by_proxy.map(|held| {
                self.public_chain_amount(
                    *origin_chain,
                    swap.intent().bridged_token,
                    held.amount,
                    cx,
                )
            }),
        };
        Some(PublicSwapLabels {
            source: source_name,
            origin: network_name(*origin_chain),
            destination: network_name(destination),
            sell: self.public_chain_amount(
                *origin_chain,
                swap.approval().sell_token,
                bounds.sell_amount,
                cx,
            ),
            sell_symbol: symbol(*origin_chain, swap.approval().sell_token),
            buy_symbol: symbol(destination, *destination_token),
            bridged_symbol: symbol(*origin_chain, swap.intent().bridged_token),
            traded: observed.trade_amounts.map(|trade| {
                format!(
                    "{} for {}",
                    self.public_chain_amount(
                        *origin_chain,
                        swap.approval().sell_token,
                        trade.sell_amount,
                        cx
                    ),
                    self.public_chain_amount(
                        *origin_chain,
                        swap.intent().bridged_token,
                        trade.buy_amount,
                        cx
                    )
                )
            }),
            deposited,
            received,
            held,
            held_named: match observed.bridge_outcome {
                Some(SwapBridgeOutcome::HeldOnDestination { .. }) => self
                    .chain_token_metadata(destination, *destination_token, cx)
                    .is_some(),
                _ => self
                    .chain_token_metadata(*origin_chain, swap.intent().bridged_token, cx)
                    .is_some(),
            },
            minimum: self.public_chain_amount(
                destination,
                *destination_token,
                bounds.private_minimum,
                cx,
            ),
            account: format!(
                "#{} · {}{}",
                record.index(),
                record
                    .address()
                    .map_or_else(|| "Address being derived".to_owned(), short_receiver),
                if claimed.is_fresh() { "" } else { " · reused" }
            ),
            setup_block: if claimed.is_fresh() {
                swap_setup_block(record)
            } else {
                None
            },
        })
    }

    pub(super) fn public_progress_title(&self, identity: SwapIdentity, cx: &App) -> String {
        self.public_swap_record(identity)
            .and_then(|(record, claimed, swap)| {
                let labels = self.public_labels(record, claimed, cx)?;
                Some(if swap.intent().order {
                    format!("Swap {} for {}", labels.sell, labels.buy_symbol)
                } else {
                    format!("Bridge {} to {}", labels.sell, labels.destination)
                })
            })
            .unwrap_or_else(|| "Swap".into())
    }

    pub(super) fn render_public_detail(
        &self,
        identity: SwapIdentity,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let Some((record, claimed, swap)) = self.public_swap_record(identity) else {
            return (
                div().child(app_muted_text(
                    "Load the destination network to view this swap.",
                )),
                None,
            );
        };
        let Some(labels) = self.public_labels(record, claimed, cx) else {
            return (div(), None);
        };
        let Some(stage) = public_swap_stage(
            record,
            claimed,
            None,
            now_unix(),
            self.attribution(record),
            self.public_destination_balance(record, claimed),
        ) else {
            return (div(), None);
        };
        let SwapUseRole::PublicSourceDestination {
            source,
            destination_token,
            ..
        } = claimed.role()
        else {
            return (div(), None);
        };
        let source = *source;
        let key = format!(
            "{}-{}",
            identity.operation.opaque_id(),
            identity.swap_use.opaque_id()
        );
        let mut steps = public_swap_steps(claimed, stage, &labels);
        // The wallet is working on this swap, in a job or between two of them. A pending step
        // names what it waits for. Any other work is on the next step, which is shown as
        // running with that work as its second line.
        let at_work = self.public_swap_at_work() == Some(identity);
        let mut status = self.public_status.clone().filter(|_| {
            at_work
                && !steps
                    .iter()
                    .any(|step| step.status == PublicActionStepStatus::Pending)
        });
        if let Some(step) = steps
            .iter_mut()
            .find(|step| step.status == PublicActionStepStatus::NotStarted)
            && let Some(working) = status.take()
        {
            step.status = PublicActionStepStatus::Pending;
            step.detail = if step.detail.is_empty() {
                working.to_string()
            } else {
                format!("{} · {working}", step.detail)
            };
        }
        let steps = steps.into_iter().enumerate().map(|(ix, step)| {
            let detail = step.detail.clone();
            progress_group(&step, detail, format!("public-swap-{key}-{ix}"))
        });
        let observed = swap.observations();
        let source_name = self.public_source_name(source, cx);
        // The minimum follows the received amount, which names the token.
        let minimum = self
            .public_record_chain(record)
            .and_then(|chain| self.chain_token_metadata(chain, *destination_token, cx))
            .map_or_else(String::new, |metadata| {
                format!(
                    "(minimum {})",
                    railgun_ui::format_token_amount(
                        swap.approval().bounds.private_minimum,
                        metadata.decimals
                    )
                )
            });
        let received = |label: &'static str| {
            labels
                .received
                .clone()
                .map(|received| fact_row(label, amount_with_note(received, minimum.clone())))
        };
        let hash = |label: &'static str, hash: String, tooltip: &'static str| {
            let copy_id = SharedString::from(format!("public-swap-{key}-{label}-copy"));
            hash_row(label, hash, copy_id, tooltip)
        };
        let fill = match observed.bridge_outcome {
            Some(
                SwapBridgeOutcome::DeliveredVerified { block, .. }
                | SwapBridgeOutcome::HeldOnDestination { block, .. },
            ) => Some(block.number),
            _ => None,
        };
        let row = |fact: &PublicSwapFact| match *fact {
            PublicSwapFact::Received => received("Received"),
            PublicSwapFact::ReportedAmount => received("Reported amount"),
            PublicSwapFact::PaidFrom { copy } => {
                Some(account_row("Paid from", source_name.clone(), source, copy))
            }
            PublicSwapFact::ControlledBy => Some(account_row(
                "Controlled by",
                source_name.clone(),
                source,
                false,
            )),
            PublicSwapFact::Destination => Some(fact_row(
                "Destination",
                app_text(format!("Private balance on {}", labels.destination)),
            )),
            PublicSwapFact::Started => claimed
                .started_at()
                .map(|at| fact_row("Started", app_text(local_date_time_label(at)))),
            PublicSwapFact::Provider => Some(fact_row(
                "Provider",
                app_text(provider_name(BridgeProvider::Across)),
            )),
            // A fact, not a link: looking the block up is the user's choice.
            PublicSwapFact::Fill => fill.map(|block| {
                fact_row(
                    "Fill",
                    app_text(format!("{} {}", labels.destination, block_label(block))),
                )
            }),
            PublicSwapFact::Deposit => observed.bridge_handoff.map(|handoff| {
                handoff.observation.transaction_hash.map_or_else(
                    || {
                        fact_row(
                            "Deposit",
                            app_text(block_label(handoff.observation.block.number)),
                        )
                    },
                    |deposit| {
                        hash(
                            "Deposit",
                            deposit.to_string(),
                            "Copy deposit transaction hash",
                        )
                    },
                )
            }),
            PublicSwapFact::DepositId => observed
                .bridge_handoff
                .and_then(|handoff| handoff.deposit_id)
                .map(|id| hash("Deposit ID", id.to_string(), "Copy Across deposit ID")),
            PublicSwapFact::DepositExpired => swap.bridge().map(|bridge| {
                fact_row(
                    "Deposit expired",
                    app_text(local_date_time_label(u64::from(bridge.fill_deadline))),
                )
            }),
            PublicSwapFact::CowProxy => swap
                .order()
                .map(|order| address_row("CoW proxy", order.proxy(), "Copy CoW proxy address")),
            PublicSwapFact::Settlement => observed
                .traded
                .and_then(|traded| traded.transaction_hash)
                .map(|settlement| {
                    hash(
                        "Settlement",
                        settlement.to_string(),
                        "Copy settlement transaction hash",
                    )
                }),
        };
        let facts = public_swap_facts(stage).iter().filter_map(row);
        let mut actions = public_swap_actions(swap, claimed, stage, now_unix());
        if at_work {
            // The wallet continues this swap by itself.
            actions.retain(|action| {
                !matches!(
                    action,
                    PublicSwapAction::Continue | PublicSwapAction::RetrySetup
                )
            });
        }
        let note = public_swap_note(swap, stage, &actions, &labels, now_unix());
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(render_submission_progress_groups(steps))
            .children(status.map(|status| app_muted_text(status).whitespace_normal()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .children(facts),
            )
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
            // One failed check is retried on the next pass and says nothing. Several in a row
            // explain why the steps above have stopped moving.
            .children(
                self.public_tracking_failures
                    .get(&(identity.operation, identity.swap_use))
                    .is_some_and(|failures| *failures >= PUBLIC_TRACKING_FAILURES_SHOWN)
                    .then(|| {
                        app_muted_text("Can't check this swap's status right now. Still trying.")
                            .whitespace_normal()
                    }),
            )
            .children(self.error.as_ref().map(|error| {
                Alert::error("public-swap-progress-error", error.clone())
                    .small()
                    .min_w_0()
            }));
        let footer = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .children(actions.into_iter().map(|action| {
                let label = match action {
                    PublicSwapAction::CheckStatus => "Check status".to_owned(),
                    PublicSwapAction::Withdraw => format!("Withdraw to {}…", labels.source),
                    PublicSwapAction::RecoverDestination => "Recover…".into(),
                    PublicSwapAction::CancelOrder => "Cancel order…".into(),
                    PublicSwapAction::RetrySetup => "Retry setup…".into(),
                    PublicSwapAction::Retry => "Retry…".into(),
                    PublicSwapAction::Continue => "Continue…".into(),
                    PublicSwapAction::CancelPreparation => "Cancel preparation".into(),
                };
                let button = app_button(
                    SharedString::from(format!("public-swap-action-{action:?}")),
                    label,
                )
                .small()
                .flex_none()
                .disabled(self.busy())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.perform_public_swap_action(identity, action, window, cx);
                }));
                // The private detail's variants for the same commands.
                match action {
                    PublicSwapAction::CheckStatus => button.outline(),
                    PublicSwapAction::CancelOrder => button.danger(),
                    _ => button,
                }
            }))
            .child(div().flex_1())
            .child(
                app_button("public-swap-close", "Close")
                    .small()
                    .flex_none()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_swap_dialog(window, cx)),
                    ),
            );
        (body, Some(footer))
    }

    pub(super) fn public_card_line(&self, cx: &App) -> Option<PublicCardLine> {
        let shown = self.shown_public_swaps().collect::<Vec<_>>();
        let (identity, stage) = *shown.first()?;
        let (record, claimed, swap) = self.public_swap_record(identity)?;
        let labels = self.public_labels(record, claimed, cx)?;
        let attention = shown
            .iter()
            .filter(|(_, stage)| {
                matches!(
                    stage.group(),
                    SwapOrderGroup::NeedsRecovery | SwapOrderGroup::NeedsAttention
                )
            })
            .count();
        let (title, detail) = if shown.len() == 1 {
            public_card_text(stage, swap.intent().order, &labels)
        } else {
            (
                format!("{} incoming swaps", shown.len()),
                "From Public accounts to your private balance here".into(),
            )
        };
        Some(PublicCardLine {
            title,
            detail,
            count: shown.len(),
            attention,
        })
    }
}

/// The status card's title and subtitle for one swap. A swap on its way reads as received
/// here, with what its stage waits for. Only an open order shields "when it fills".
pub(super) fn public_card_text(
    stage: PublicSwapStage,
    order: bool,
    labels: &PublicSwapLabels,
) -> (String, String) {
    let PublicSwapLabels {
        source,
        origin,
        destination,
        ..
    } = labels;
    let receiving = |next: &str| {
        (
            format!("Receiving {} from {source} on {origin}", labels.buy_symbol),
            format!("{} · {next}", stage.label()),
        )
    };
    // A title names an amount only when the wallet can format it, otherwise the token.
    let held = labels.held.as_deref().filter(|_| labels.held_named);
    let in_proxy = || {
        format!(
            "{} held by {source}'s CoW proxy on {origin}",
            held.unwrap_or(&labels.bridged_symbol)
        )
    };
    match stage {
        PublicSwapStage::Preparing(SwapStage::SetupFailed) => receiving("retry it from Details…"),
        PublicSwapStage::Preparing(_) => receiving(if order {
            "the order isn't placed yet"
        } else {
            "the deposit isn't sent yet"
        }),
        PublicSwapStage::Approving => {
            receiving(&format!("waiting for {source}'s approval to confirm"))
        }
        PublicSwapStage::ApprovalFailed => {
            receiving(&format!("{} stays in {source}", labels.sell_symbol))
        }
        PublicSwapStage::OrderOpen => {
            receiving("shielding to your private balance here when it fills")
        }
        PublicSwapStage::SubmissionPending => receiving("CoW hasn't accepted the order yet"),
        PublicSwapStage::SubmissionRejected => receiving("CoW didn't accept the order"),
        PublicSwapStage::Depositing => receiving(&format!("waiting for confirmation on {origin}")),
        PublicSwapStage::DepositFailed => receiving("nothing was bridged"),
        PublicSwapStage::Bridging => {
            receiving("shielding to your private balance here when a relayer delivers it")
        }
        PublicSwapStage::DeliveredVerified => receiving("shielded to your private balance here"),
        PublicSwapStage::DeliveredReported => {
            receiving("the wallet couldn't verify the shield here")
        }
        PublicSwapStage::NeedsAttention => receiving("check the deposit's status in Details…"),
        PublicSwapStage::Refunding => (
            format!("Refunding to {source} on {origin}"),
            format!(
                "The deposit expired · Across returns {}, usually within a few hours",
                labels
                    .deposited
                    .as_deref()
                    .unwrap_or(&labels.bridged_symbol)
            ),
        ),
        PublicSwapStage::Refunded => (
            format!("Refunded to {source} on {origin}"),
            "The deposit expired · nothing to recover".into(),
        ),
        PublicSwapStage::HeldByProxy => (
            in_proxy(),
            format!("The bridge deposit didn't run · withdraw it to {source}"),
        ),
        PublicSwapStage::Withdrawing => (
            in_proxy(),
            format!("Withdrawing to {source} · waiting for confirmation"),
        ),
        PublicSwapStage::WithdrawnPendingCheck => (
            format!("Withdrawn to {source} on {origin}"),
            "Checking for a late bridge deposit before the swap ends".into(),
        ),
        PublicSwapStage::SwappedNotBridged => (
            format!("Withdrawn to {source} on {origin}"),
            "Swapped without bridging · nothing arrives here".into(),
        ),
        PublicSwapStage::HeldOnDestination => (
            format!(
                "{} held on {destination}",
                held.unwrap_or(&labels.buy_symbol)
            ),
            "The shield didn't run · recover it from its stealth account here".into(),
        ),
        PublicSwapStage::Recovered => (
            format!("Recovered on {destination}"),
            "Shielded to your private balance here".into(),
        ),
        PublicSwapStage::Cancelled | PublicSwapStage::Expired => (
            stage.label().to_owned(),
            format!(
                "Nothing traded · {} stays in {source} on {origin}",
                labels.sell_symbol
            ),
        ),
        PublicSwapStage::PreparationCancelled => (
            stage.label().to_owned(),
            "Nothing was swapped or bridged".into(),
        ),
    }
}

/// The note under the facts: what waiting or acting costs, naming the account, the token, the
/// time and the network.
pub(super) fn public_swap_note(
    swap: &PublicSwapRecord,
    stage: PublicSwapStage,
    actions: &[PublicSwapAction],
    labels: &PublicSwapLabels,
    now: u64,
) -> Option<String> {
    let PublicSwapLabels {
        source,
        origin,
        bridged_symbol,
        ..
    } = labels;
    match stage {
        PublicSwapStage::OrderOpen
        | PublicSwapStage::SubmissionPending
        | PublicSwapStage::SubmissionRejected
            if actions.contains(&PublicSwapAction::CancelOrder) =>
        {
            swap.order().map(|order| {
                format!(
                    "The order expires by itself at {} at no cost. Cancelling now sends a transaction from {source} and costs gas.",
                    local_time_label(u64::from(order.valid_to()))
                )
            })
        }
        PublicSwapStage::HeldByProxy => Some(format!(
            "Withdraw… sends one transaction from {source} that moves the {bridged_symbol} from the proxy to {source}. It costs gas on {origin}. You can then bridge it from {source} as {bridged_symbol}."
        )),
        PublicSwapStage::Refunding | PublicSwapStage::Refunded => {
            let arrives = if stage == PublicSwapStage::Refunded {
                "arrived"
            } else {
                "arrives"
            };
            // A direct deposit sold the token it gets back.
            Some(if swap.intent().order {
                format!(
                    "The refund is {bridged_symbol}, not the {} you sold. It {arrives} in {source} with nothing to recover.",
                    labels.sell_symbol
                )
            } else {
                format!("The refund {arrives} in {source} with nothing to recover.")
            })
        }
        // A retry waits until the order can no longer fill and its hook batch can no longer run.
        PublicSwapStage::Cancelled | PublicSwapStage::Expired => swap.order().map(|order| {
            let free = u64::from(order.valid_to().max(order.batch().deadline()));
            if now > free {
                "Nothing traded.".to_owned()
            } else {
                format!(
                    "Nothing traded. You can retry after {}, once the order and its bridge instructions can no longer run.",
                    local_time_label(free)
                )
            }
        }),
        PublicSwapStage::PreparationCancelled => Some(format!(
            "{}. If the stealth account on {} already signed its shield for this swap, it stays reserved until that shield can no longer run.",
            if swap.intent().order {
                "No order was placed"
            } else {
                "Nothing was deposited"
            },
            labels.destination
        )),
        _ => None,
    }
}

/// The available commands follow durable outcomes; refunds never offer origin recovery.
pub(super) fn public_swap_actions(
    swap: &PublicSwapRecord,
    claimed: &SwapUseRecord,
    stage: PublicSwapStage,
    now: u64,
) -> Vec<PublicSwapAction> {
    use wallet_ops::vault::{PublicSwapPath, PublicSwapTransactionKind};
    if matches!(
        stage,
        PublicSwapStage::DeliveredVerified
            | PublicSwapStage::Refunded
            | PublicSwapStage::Recovered
            | PublicSwapStage::SwappedNotBridged
            | PublicSwapStage::PreparationCancelled
    ) {
        return Vec::new();
    }
    let cancellable = matches!(
        stage,
        PublicSwapStage::OrderOpen
            | PublicSwapStage::SubmissionPending
            | PublicSwapStage::SubmissionRejected
    ) && swap.order_can_fill(now)
        && !swap
            .transactions()
            .iter()
            .any(|tx| tx.kind == PublicSwapTransactionKind::Invalidation && tx.inclusion.is_none());
    // An accepted order that can still fill offers only its cancellation.
    let mut actions = if cancellable && stage == PublicSwapStage::OrderOpen {
        Vec::new()
    } else {
        vec![PublicSwapAction::CheckStatus]
    };
    match stage {
        PublicSwapStage::HeldByProxy => actions.push(PublicSwapAction::Withdraw),
        PublicSwapStage::HeldOnDestination => actions.push(PublicSwapAction::RecoverDestination),
        _ if cancellable => actions.push(PublicSwapAction::CancelOrder),
        PublicSwapStage::Cancelled | PublicSwapStage::Expired
            if public_swap_can_retry(swap, now) =>
        {
            actions.push(PublicSwapAction::Retry);
        }
        PublicSwapStage::Preparing(SwapStage::SetupFailed) => {
            actions.push(PublicSwapAction::RetrySetup);
        }
        PublicSwapStage::Preparing(
            SwapStage::Ready
            | SwapStage::Approved
            | SwapStage::SetupPending
            | SwapStage::SetupNotSent,
        )
        | PublicSwapStage::ApprovalFailed
            if !claimed.is_stopped()
                && (swap.path().is_none()
                    || (matches!(swap.path(), Some(PublicSwapPath::Deposit))
                        && !swap
                            .transactions()
                            .iter()
                            .any(|tx| tx.kind == PublicSwapTransactionKind::Deposit))) =>
        {
            actions.push(PublicSwapAction::Continue);
        }
        _ => {}
    }
    let unsigned = swap.path().is_none()
        || (matches!(swap.path(), Some(PublicSwapPath::Deposit))
            && !swap
                .transactions()
                .iter()
                .any(|tx| tx.kind == PublicSwapTransactionKind::Deposit));
    if unsigned && !claimed.is_stopped() {
        actions.push(PublicSwapAction::CancelPreparation);
    }
    actions
}

/// A fresh review starts only after both signed authorizations have expired.
pub(super) fn public_swap_can_retry(swap: &PublicSwapRecord, now: u64) -> bool {
    let observed = swap.observations();
    observed.traded.is_none()
        && observed.bridge_handoff.is_none()
        && !swap.proxy_holds_proceeds()
        && (observed.cancelled.is_some() || observed.expired.is_some())
        && swap.order().is_some_and(|order| {
            now > u64::from(order.valid_to()) && now > u64::from(order.batch().deadline())
        })
}

/// A Public account as a fact, laid out like the private detail's receiver: its label when it
/// has one, then its address, shortened, and optionally a control that copies it in full.
fn account_row(
    label: &'static str,
    name: Option<String>,
    address: Address,
    copy: bool,
) -> gpui::Div {
    let short = short_receiver(address);
    let copy = copy.then(|| {
        let address = address.to_checksum(None);
        let copy_id = SharedString::from(format!("public-swap-account-{address}-copy"));
        clipboard_with_toast(copy_id, address).tooltip("Copy Public account address")
    });
    fact_row(
        label,
        div()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .children(name.map(|name| app_text(name).min_w_0().truncate()))
            .child(
                app_strong_text(short)
                    .flex_none()
                    .font_family(theme::APP_MONO_FONT_FAMILY),
            )
            .children(copy),
    )
}
