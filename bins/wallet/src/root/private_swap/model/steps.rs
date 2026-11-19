#[allow(clippy::wildcard_imports)]
use super::*;

impl SwapStep {
    pub(super) fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        status: PublicActionStepStatus,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            status,
            children: Vec::new(),
            account: None,
            order: None,
            transaction: None,
            destination_transaction: None,
            block: None,
        }
    }

    fn with_destination_transaction(mut self, transaction: Option<String>) -> Self {
        self.destination_transaction = transaction;
        self
    }

    pub(super) const fn in_block(mut self, block: Option<u64>) -> Self {
        self.block = block;
        self
    }

    pub(super) const fn with_order(mut self, order: Option<OrderUid>) -> Self {
        self.order = order;
        self
    }

    pub(super) const fn with_transaction(mut self, transaction: Option<B256>) -> Self {
        self.transaction = transaction;
        self
    }
}

/// A step's status from its sub-steps: failed if any failed, done once all are,
/// and otherwise in progress.
pub(in crate::root) fn parent_step_status(children: &[SwapStep]) -> PublicActionStepStatus {
    use PublicActionStepStatus::{Done, Error, Pending};
    let all = |status| children.iter().all(|child| child.status == status);
    if children.iter().any(|child| child.status == Error) {
        Error
    } else if all(Done) {
        Done
    } else {
        Pending
    }
}

/// The progress steps: set up, order open, traded, back in private balance. An
/// attempt that ended, or stranded funds before a trade, ends the list at the order step. A
/// fill the orderbook reports shows every step its settlement performs as confirming until
/// observation records it, with one confirmation count, on the trade, and the private return
/// tells whether the reported trade block shows its shield. A Public address swap has
/// no private-balance step, as [`external_steps`] describes, and a Bridge swap hands off to its
/// bridge instead, as [`bridge_steps`] describes. Once the swap has an order, the order step
/// carries its ID, and the private-balance step carries its shield transaction once the reported
/// trade block or verified delivery shows it.
pub(in crate::root) fn swap_steps(stage: SwapStage, labels: &SwapLabels) -> Vec<SwapStep> {
    let mut steps = if let Some(bridge) = &labels.bridge {
        let mut steps = bridge_steps(stage, labels, bridge);
        if let (Some(private), Some(setup)) = (&bridge.private, steps.first_mut()) {
            *setup = private_setup_step(stage, private, setup);
        }
        steps
    } else {
        let steps = reshield_steps(stage, labels);
        match &labels.receiver {
            Some(receiver) => external_steps(steps, receiver, labels),
            None => steps,
        }
    };
    // The order step follows the setup in every stage that has an order.
    if matches!(
        stage,
        SwapStage::SubmissionPending | SwapStage::SubmissionRejected | SwapStage::Order(_)
    ) && let Some(step) = steps.get_mut(1)
    {
        step.order = labels.order;
    }
    steps
}

fn reshield_steps(stage: SwapStage, labels: &SwapLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let step = |label: &str, detail: String, status| SwapStep::new(label, detail, status);
    let setup = |status, detail: &str| step("Stealth account set up", detail.to_owned(), status);
    let open = |status, detail: String| step("Order open", detail, status);
    let traded = |status| step(TRADED, String::new(), status);
    let delivered = |status, detail: String| step(BACK_IN_PRIVATE_BALANCE, detail, status);
    let expiry = || {
        labels.expires.as_ref().map_or_else(String::new, |at| {
            if labels.lapsed {
                format!("Expired at {at} · awaiting confirmation")
            } else {
                format!("Expires {at}")
            }
        })
    };
    match stage {
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.fill_hint.is_some() =>
        {
            vec![
                setup(Done, ""),
                open(Done, String::new()),
                step(
                    TRADED,
                    labels
                        .fill_hint
                        .as_ref()
                        .map_or_else(String::new, SwapFillHint::traded),
                    Pending,
                ),
                // A shield the reported block shows is done, as a private Bridge delivery's
                // is, while the trade above still confirms.
                delivered(
                    if labels
                        .fill_hint
                        .as_ref()
                        .is_some_and(|hint| matches!(hint.shield, SwapSeen::Detected(_)))
                    {
                        Done
                    } else {
                        Pending
                    },
                    labels
                        .fill_hint
                        .as_ref()
                        .map_or_else(String::new, SwapFillHint::shielded),
                )
                .with_transaction(
                    labels
                        .fill_hint
                        .as_ref()
                        .and_then(|hint| hint.shield_transaction),
                ),
            ]
        }
        SwapStage::SetupNotSent => vec![
            setup(NotStarted, "Not sent yet"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupRetired => vec![
            setup(
                Warning,
                "This stealth account has previous activity, so it can't be used for the swap.",
            ),
            open(NotStarted, "No order was placed".into()),
        ],
        SwapStage::SetupSubmitting => vec![
            setup(Pending, "Sending the setup through a broadcaster"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupPending => vec![
            setup(Pending, "Waiting for confirmation"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupFailed => vec![
            setup(
                Warning,
                "The setup didn't take effect. Nothing was unshielded.",
            ),
            open(NotStarted, String::new()),
        ],
        SwapStage::Ready => vec![
            setup(Done, ""),
            open(NotStarted, "Review a quote to place the order".into()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Approved => vec![
            setup(Done, ""),
            open(NotStarted, "Confirm to place the order".into()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SubmissionPending | SwapStage::SubmissionRejected => vec![
            setup(Done, ""),
            step(
                "Order submission",
                if stage == SwapStage::SubmissionPending {
                    format!(
                        "Acceptance is unconfirmed. Submit the same order again. {}",
                        expiry()
                    )
                } else {
                    format!(
                        "This order was rejected. Retry after its expiry is final. {}",
                        expiry()
                    )
                },
                Warning,
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::Open) => vec![
            setup(Done, ""),
            open(
                Pending,
                [
                    String::from(if labels.lapsed {
                        ""
                    } else {
                        "Waiting for a solver"
                    }),
                    expiry(),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => vec![
            setup(Done, ""),
            open(
                Pending,
                format!(
                    "{} is unshielded{}. {}",
                    labels.sell,
                    if labels.lapsed {
                        ""
                    } else {
                        " and waiting for a solver"
                    },
                    expiry()
                )
                .trim_end()
                .to_owned(),
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }) => vec![
            setup(Done, ""),
            step(
                "Unshielded, not traded",
                format!(
                    "{} is in the stealth account. The order expired without a fill.",
                    labels.sell
                ),
                Warning,
            ),
        ],
        // The traded step shows the settlement where no later step does. A finished return's
        // shield transaction is that settlement.
        SwapStage::Order(SwapOrderState::Traded) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done).with_transaction(labels.settlement_transaction),
            delivered(
                Pending,
                "Private return unverified. Check status for retry or recovery.".into(),
            ),
        ],
        SwapStage::Order(SwapOrderState::Done) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done),
            delivered(Done, "Finalized".into()).with_transaction(labels.shield_transaction),
        ],
        SwapStage::Order(SwapOrderState::NotDelivered) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done).with_transaction(labels.settlement_transaction),
            step(
                "Not delivered",
                format!(
                    "{} is in the stealth account. It wasn't moved to your private balance.",
                    labels.buy_symbol
                ),
                Warning,
            ),
        ],
        // Only Bridge orders reach these states; [`bridge_steps`] adds their hand-off and
        // outcome.
        SwapStage::Order(
            SwapOrderState::Bridging
            | SwapOrderState::Refunding
            | SwapOrderState::NeedsAttention
            | SwapOrderState::HeldOnDestination,
        ) => vec![setup(Done, ""), open(Done, String::new())],
        SwapStage::Order(SwapOrderState::Superseded) => vec![
            setup(Done, ""),
            step(
                "Not filled",
                "Another order of this swap filled.".into(),
                Warning,
            ),
        ],
        // An unfilled expiry is a normal result of a tight minimum, so it's a warning with
        // its likely reason, not an error.
        SwapStage::Order(SwapOrderState::AttemptEnded(cause)) => vec![
            setup(Done, ""),
            step(
                match cause {
                    SwapPreHookDeathCause::Expired => "Not filled",
                    SwapPreHookDeathCause::Cancellation => "Order cancelled",
                    SwapPreHookDeathCause::Recovery
                    | SwapPreHookDeathCause::OlderPostHook
                    | SwapPreHookDeathCause::Unknown => "Order ended",
                },
                if cause == SwapPreHookDeathCause::Expired {
                    let at = labels
                        .expires
                        .as_ref()
                        .map_or_else(String::new, |at| format!(" at {at}"));
                    match &labels.uncovered_gas {
                        Some(rest) => format!(
                            "No solver covered {rest} of the gas before the order expired{at}."
                        ),
                        None => format!("No solver filled it before the order expired{at}."),
                    }
                } else {
                    "Nothing was unshielded.".into()
                },
                Warning,
            ),
        ],
        SwapStage::Recovered => vec![
            setup(Done, ""),
            step(
                "Recovered",
                "Recovery returned the funds to your private balance.".into(),
                Done,
            ),
        ],
    }
}

/// A Public address swap's steps. The settlement that trades also pays the receiver, so one
/// step, delivered to the receiver, replaces Traded and Back in private balance: the least the
/// receiver gets while the order can fill, then what it got. On a reported fill it confirms with
/// the trade's count and reported amount.
fn external_steps(steps: Vec<SwapStep>, receiver: &str, labels: &SwapLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, Pending};
    let traded = steps
        .iter()
        .find(|step| step.label == TRADED)
        .map(|step| (step.status, step.detail.clone()));
    steps
        .into_iter()
        .filter(|step| step.label != TRADED)
        .map(|mut step| {
            if step.label != BACK_IN_PRIVATE_BALANCE {
                return step;
            }
            step.label = format!("Delivered to {receiver}");
            // A reported fill confirms here, since no separate trade step shows it. Only a
            // reported fill leaves the trade in progress.
            if let Some((status, detail)) = traded.clone().filter(|(status, _)| *status == Pending)
            {
                step.status = status;
                step.detail = detail;
            }
            // The settlement that traded also delivered.
            if step.status == Done {
                step.transaction = labels.settlement_transaction;
            }
            step.detail = match (&labels.received, &labels.minimum) {
                (Some(received), _) if step.status == Done => {
                    format!("{} → {received} · Finalized", labels.sell)
                }
                (_, Some(minimum)) if step.status != Done && step.detail.is_empty() => {
                    format!("At least {minimum}")
                }
                _ => step.detail,
            };
            step
        })
        .collect()
}

/// A private Bridge swap's account step with one sub-step per network, each naming its stealth
/// account. An account the swap sets up shows the block of its setup or what it waits for. An
/// account the swap reuses shows that it is reused and ready, with no setup of its own, and
/// never as pending. The step is "Stealth accounts set up", or "Stealth accounts ready" when the
/// swap sets up neither. Its own status follows its sub-steps, as [`parent_step_status`]
/// derives it.
fn private_setup_step(
    stage: SwapStage,
    private: &SwapPrivateBridgeLabels,
    setup: &SwapStep,
) -> SwapStep {
    use PublicActionStepStatus::{Done, Error, NotStarted, Pending};
    let children = private
        .setups
        .iter()
        .map(|account| {
            let (status, detail) = match account.progress {
                SwapSetupProgress::Done if account.reused => (Done, REUSED_READY.to_owned()),
                // Its records are read once its network is loaded. Nothing is on its way.
                SwapSetupProgress::NetworkLoading if account.reused => (
                    NotStarted,
                    format!("Reused · checked once {} loads", account.network),
                ),
                SwapSetupProgress::NotSent => (NotStarted, "Not sent yet".to_owned()),
                SwapSetupProgress::NetworkLoading => {
                    (Pending, format!("Waiting for {} to load…", account.network))
                }
                SwapSetupProgress::Submitting => (
                    Pending,
                    account
                        .detail
                        .clone()
                        .unwrap_or_else(|| "Submitting…".into()),
                ),
                SwapSetupProgress::Pending => (
                    Pending,
                    account
                        .detail
                        .clone()
                        .unwrap_or_else(|| "Waiting for broadcaster…".into()),
                ),
                SwapSetupProgress::Done => (Done, String::new()),
                SwapSetupProgress::Failed => (Error, "Not confirmed".to_owned()),
            };
            SwapStep {
                account: account.account,
                ..SwapStep::new(account.network.clone(), detail, status)
            }
        })
        .collect::<Vec<_>>();
    let status = parent_step_status(&children);
    let failed = children
        .iter()
        .filter(|child| child.status == Error)
        .map(|child| child.label.as_str())
        .collect::<Vec<_>>();
    let detail = match stage {
        // A retired account keeps the reason its swap can't continue.
        SwapStage::SetupRetired => setup.detail.clone(),
        _ if failed.is_empty() => String::new(),
        _ => format!(
            "The setup on {} wasn't confirmed. Nothing was unshielded.",
            failed.join(" and ")
        ),
    };
    let label = if private.setups.iter().all(|account| account.reused) {
        "Stealth accounts ready"
    } else {
        "Stealth accounts set up"
    };
    SwapStep {
        children,
        ..SwapStep::new(label, detail, status)
    }
}

/// A Bridge swap's steps: set up, order open, sent to the bridge once the hand-off is proven,
/// then delivered on the destination network, labelled by the recorded outcome: verified, or
/// reported by the provider. A fill the orderbook reports adds the trade, which confirms with
/// the bridge deposit under the trade's count. The deposit tells whether the reported trade
/// block shows it, and the last step then shows what the provider reports about the delivery,
/// marked as reported. A private delivery's reported step waits for its fill block on the
/// destination network, which shows the shield or the proceeds held by the destination stealth
/// account. Refunding and Needs attention replace the
/// delivered step. An Across post-hook that didn't run replaces both with the bought token held
/// on the swap's network, once an account read found it there. Until then an Across swap to a
/// public receiver shows its deposit as unverified. A private Bridge swap's last step is its
/// shield into the private balance there, and proceeds its destination stealth account holds
/// replace that step.
fn bridge_steps(stage: SwapStage, labels: &SwapLabels, bridge: &SwapBridgeLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let step = |label: String, detail: String, status| SwapStep::new(label, detail, status);
    let provider = provider_name(bridge.provider);
    let sent = bridge.sent.as_ref().unwrap_or(&labels.buy_symbol);
    let handed_off = || {
        step(
            BRIDGE_DEPOSIT.into(),
            bridge
                .sent
                .as_ref()
                .map_or_else(String::new, |sent| match bridge.provider {
                    BridgeProvider::Across => format!("{sent} to Across"),
                    BridgeProvider::NearIntents => format!("{sent} to 1Click"),
                }),
            Done,
        )
        .with_transaction(bridge.handoff_transaction)
    };
    let private = bridge.private.as_ref();
    // Proceeds the destination stealth account holds, `held` of them when the amount is known.
    let held_detail = |held: Option<&String>| {
        let token = &bridge.token;
        let account = private
            .and_then(|private| private.setups[1].account?.index)
            .map_or_else(
                || "the stealth account".to_owned(),
                |index| format!("stealth account #{index}"),
            );
        format!(
            "{provider} delivered {}, but the shield didn't run. The {token} is in {account} on {}.",
            held.unwrap_or(token),
            bridge.network
        )
    };
    let delivered = if private.is_some() {
        format!("Private on {}", bridge.network)
    } else {
        format!("Delivered on {}", bridge.network)
    };
    // Across delivers its deposit's exact output; NEAR Intents converts all of it.
    let expected = match (&bridge.minimum, bridge.provider) {
        (Some(minimum), BridgeProvider::Across) => {
            format!("{minimum} to {}", bridge.destination())
        }
        (Some(minimum), BridgeProvider::NearIntents) => {
            format!("At least {minimum} to {}", bridge.destination())
        }
        (None, _) => format!("To {}", bridge.destination()),
    };
    // What the delivered step shows while the provider has the deposit.
    let in_transit = format!(
        "{expected} · {}",
        match bridge.provider {
            BridgeProvider::Across => "Across usually fills within a few minutes",
            BridgeProvider::NearIntents => "NEAR Intents usually delivers within a few minutes",
        }
    );
    let mut steps = reshield_steps(stage, labels);
    let outcome = match stage {
        // The reported settlement trades and hands off at once, so both confirm together. The
        // provider's report of the delivery is its word until the settlement is verified.
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.fill_hint.is_some() =>
        {
            vec![
                step(
                    TRADED.into(),
                    labels
                        .fill_hint
                        .as_ref()
                        .map_or_else(String::new, SwapFillHint::traded),
                    Pending,
                ),
                step(
                    BRIDGE_DEPOSIT.into(),
                    match &bridge.deposit {
                        SwapSeen::Looking => "Looking for the deposit…".into(),
                        SwapSeen::Detected(Some(sent)) => match bridge.provider {
                            BridgeProvider::Across => {
                                format!("Deposit detected · {sent} to Across")
                            }
                            BridgeProvider::NearIntents => {
                                format!("Deposit detected · {sent} to 1Click")
                            }
                        },
                        SwapSeen::Detected(None) => "Deposit detected".into(),
                        SwapSeen::NotFound => "No deposit detected yet".into(),
                    },
                    Pending,
                ),
                match &bridge.report {
                    None => step(delivered, expected, NotStarted),
                    Some(SwapBridgeReported::Pending) => step(delivered, in_transit, Pending),
                    Some(SwapBridgeReported::Delivered {
                        amount,
                        shield,
                        transaction,
                    }) => {
                        let reported =
                            format!("Delivered on {} · reported by {provider}", bridge.network);
                        // A private delivery follows what its fill block shows of the shield.
                        let delivery = match (private, shield) {
                            (None, _) => step(
                                reported,
                                amount.as_ref().map_or_else(
                                    || format!("To {}", bridge.receiver),
                                    |amount| format!("{amount} to {}", bridge.receiver),
                                ),
                                Done,
                            ),
                            (Some(_), SwapShieldSeen::Looking) => step(
                                reported,
                                amount.as_ref().map_or_else(
                                    || "Looking for the shield…".into(),
                                    |amount| {
                                        format!("{amount} delivered · looking for the shield…")
                                    },
                                ),
                                Pending,
                            ),
                            (Some(_), SwapShieldSeen::Detected(credited)) => {
                                step(delivered, format!("Shield detected · {credited}"), Done)
                            }
                            (Some(_), SwapShieldSeen::Held(held)) => step(
                                format!("Held by stealth account on {}", bridge.network),
                                held_detail(Some(held)),
                                Warning,
                            ),
                        };
                        delivery.with_transaction(*transaction)
                    }
                    Some(SwapBridgeReported::Refunded) => step(
                        format!("Refund · reported by {provider}"),
                        format!(
                            "{provider} reports the deposit refunded or expired. The wallet checks it once the settlement is verified."
                        ),
                        Warning,
                    ),
                    Some(SwapBridgeReported::Failed) => step(
                        format!("Problem · reported by {provider}"),
                        format!(
                            "{provider} reports the deposit as failed or incomplete. The wallet checks it once the settlement is verified."
                        ),
                        Warning,
                    ),
                },
            ]
        }
        // An Across settlement that deposits shows the deposit with the trade, so a trade
        // without one leaves the hand-off unverified. Only an account read tells where the
        // bought token is.
        SwapStage::Order(SwapOrderState::Traded)
            if bridge.provider == BridgeProvider::Across && private.is_none() =>
        {
            vec![
                step(
                    "Bridge deposit unverified".into(),
                    format!(
                        "The trade is verified, but its settlement didn't show this swap's bridge deposit. Check status to see whether the bought {} is in the stealth account.",
                        labels.buy_symbol
                    ),
                    Pending,
                ),
                step(delivered, expected, NotStarted),
            ]
        }
        SwapStage::Order(SwapOrderState::Traded) => vec![
            step(
                BRIDGE_DEPOSIT.into(),
                "Checking the settlement…".into(),
                Pending,
            ),
            step(delivered, expected, NotStarted),
        ],
        SwapStage::Order(SwapOrderState::Bridging) => {
            vec![handed_off(), step(delivered, in_transit, Pending)]
        }
        SwapStage::Order(SwapOrderState::Done) => vec![
            handed_off(),
            step(
                if bridge.reported {
                    format!("{delivered} · reported by {provider}")
                } else {
                    format!("{delivered} · verified")
                },
                match (&labels.received, private) {
                    (Some(received), Some(_)) => {
                        format!("{received} shielded to {}", bridge.destination())
                    }
                    (None, Some(_)) => format!("Shielded to {}", bridge.destination()),
                    (Some(received), None) => format!("{received} to {}", bridge.receiver),
                    (None, None) => format!("To {}", bridge.receiver),
                },
                Done,
            )
            .with_transaction(bridge.delivered_transaction)
            .with_destination_transaction(bridge.destination_transaction.clone()),
        ],
        SwapStage::Order(SwapOrderState::HeldOnDestination) => vec![
            handed_off(),
            step(
                format!("Held by stealth account on {}", bridge.network),
                held_detail(private.and_then(|private| private.held.as_ref())),
                Warning,
            )
            .with_transaction(bridge.delivered_transaction),
        ],
        SwapStage::Order(SwapOrderState::Refunding) => vec![
            handed_off(),
            step(
                format!("Refunding on {}", bridge.origin),
                match bridge.provider {
                    BridgeProvider::Across => format!(
                        "No relayer filled the deposit before it expired. Across returns {sent} to the stealth account on {}, usually within a few hours.",
                        bridge.origin
                    ),
                    BridgeProvider::NearIntents => format!(
                        "NEAR Intents refunded the deposit to the stealth account on {}.",
                        bridge.origin
                    ),
                },
                Warning,
            )
            .with_transaction(bridge.refund_transaction),
        ],
        SwapStage::Order(SwapOrderState::NeedsAttention) => vec![
            handed_off(),
            step(
                "Needs attention".into(),
                format!(
                    "{provider} reported the deposit as failed or incomplete. The wallet can't recover it. {provider} has to resolve it."
                ),
                Warning,
            ),
        ],
        SwapStage::Order(SwapOrderState::NotDelivered) => vec![step(
            "Not sent to the bridge".into(),
            format!("{sent} is in the stealth account on {}.", bridge.origin),
            Warning,
        )],
        // Before the trade, the hand-off waits for it.
        _ => {
            let traded = steps
                .iter()
                .find(|step| step.label == TRADED)
                .map(|step| (step.status, step.detail.clone()));
            return steps
                .into_iter()
                .filter(|step| step.label != TRADED)
                .flat_map(|step| {
                    if step.label != BACK_IN_PRIVATE_BALANCE {
                        return vec![step];
                    }
                    let (status, detail) = traded
                        .clone()
                        .unwrap_or_else(|| (NotStarted, String::new()));
                    vec![
                        SwapStep::new(BRIDGE_DEPOSIT, detail, status),
                        SwapStep::new(delivered.clone(), expected.clone(), NotStarted),
                    ]
                })
                .collect();
        }
    };
    // Set up and Order open are done once the order traded.
    steps.truncate(2);
    steps.extend(outcome);
    steps
}
