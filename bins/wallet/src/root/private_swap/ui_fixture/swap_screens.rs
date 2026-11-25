#[allow(clippy::wildcard_imports)]
use super::*;

impl PrivateSwapsView {
    /// In a `detail:` mode, show the detail of one synthesized swap that `source` pays with
    /// `sell`, in place of the form that was just opened.
    pub(in crate::root::private_swap) fn open_ui_fixture_detail(
        &mut self,
        source: Address,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(Mode::Detail(stage)) = mode() else {
            return;
        };
        let Some((chain, identity, record)) = self.ui_fixture_record(stage, source, sell, cx)
        else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        if stage == Detail::HeldAfterPartialRecovery
            && let Some(SwapBridgeOutcome::HeldOnDestination { amount, block, .. }) = record
                .public_swap_use(identity.swap_use)
                .and_then(|(_, swap)| swap.observations().bridge_outcome)
        {
            self.public_destination_balances.insert(
                (chain, identity.operation, identity.swap_use),
                (
                    amount * U256::from(9_u8) / U256::from(10_u8),
                    BlockNumHash::new(block.number + 1, block.hash),
                ),
            );
        }
        if stage == Detail::PermitUsedUp {
            self.public_permit_used_up
                .insert((identity.operation, identity.swap_use));
        }
        self.public_records = vec![(chain, record)];
        self.navigate(SwapDialogView::PublicDetail(identity), window, cx);
    }

    /// In a `flow:` mode, stand the record of the reviewed swap `identity`, which trades
    /// `swap` and delivers on `destination`, in for the claim that would have written it.
    pub(in crate::root::private_swap) fn stage_ui_fixture_flow(
        &mut self,
        identity: SwapIdentity,
        destination: u64,
        swap: &Staged,
    ) {
        let Some(Mode::Flow(flow)) = mode() else {
            return;
        };
        let stage = match flow {
            Flow::SetupSending | Flow::Error => Detail::SetupNotSent,
            Flow::SetupWaiting => Detail::SetupPending,
            // Terms change at signing time, after the account is set up.
            Flow::Finishing | Flow::Placing | Flow::Signatures | Flow::Changed => {
                Detail::AccountReady
            }
        };
        let Some(record) = staged_record(stage, identity, swap) else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        self.public_records = vec![(destination, record)];
    }

    /// A swap at `stage` to the first other network with private swaps: an order that buys
    /// the origin's wrapped native token and delivers the destination's, so both are named.
    fn ui_fixture_record(
        &self,
        stage: Detail,
        source: Address,
        sell: Address,
        cx: &App,
    ) -> Option<(u64, SwapIdentity, ExecutorRecord)> {
        let origin = self.origin_chain_id;
        let root = self.root.upgrade()?;
        let (bridged, destination, delivered) = {
            let chains = &root.read(cx).effective_chain_configs;
            let bridged = chains.get(origin)?.wrapped_native_token?;
            let (destination, delivered) = chains.values().find_map(|chain| {
                let token = chain.wrapped_native_token?;
                (chain.chain_id != origin && chain.swap_profile().is_some())
                    .then_some((chain.chain_id, token))
            })?;
            (bridged, destination, delivered)
        };
        let amount = |chain, token, count| {
            let decimals = self
                .chain_token_metadata(chain, token, cx)
                .map_or(18, |metadata| metadata.decimals);
            ten_thousandths(decimals, count)
        };
        let identity = SwapIdentity {
            operation: ExecutorOperationId::random().ok()?,
            swap_use: SwapUseId::random().ok()?,
        };
        let record = staged_record(
            stage,
            identity,
            &Staged {
                origin,
                source,
                sell,
                bridged,
                delivered,
                order: true,
                sold: amount(origin, sell, 1_000_000),
                bought: amount(origin, bridged, 300),
                received: amount(destination, delivered, 299),
            },
        )?;
        Some((destination, identity, record))
    }
}

impl PrivateSwapsView {
    /// The estimate that stands in for a broadcaster's in the retry dialog of a
    /// `setup:retired` mode, 0.00016 WETH, so the dialog can open its review on a wallet
    /// without private funds. `None` in every other mode.
    pub(in crate::root::private_swap) fn ui_fixture_setup_estimate(
        &self,
        cx: &App,
    ) -> Option<ExecutorRecoveryFeeEstimate> {
        if !matches!(mode(), Some(Mode::Setup(Setup::Retired))) {
            return None;
        }
        let root = self.root.upgrade()?;
        let delegate = root
            .read(cx)
            .effective_chain_configs
            .get(SWAP_ORIGIN)?
            .accepted_executor_profile()?
            .delegate();
        Some(ExecutorRecoveryFeeEstimate::for_ui_fixture(
            SWAP_ORIGIN,
            WETH,
            delegate,
            U256::from(SETUP_RETRY_FEE),
        ))
    }

    /// In a `setup:` mode, show the detail of the synthesized swap with its setup held at
    /// `setup`: the records of its accounts, and the stages this session would hold of each
    /// setup. A running setup has a job that never answers.
    fn open_ui_fixture_setup(
        &mut self,
        setup: Setup,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use TransactionGenerationStage::{
            EstimatingBroadcasterFee, GeneratingPoiProofs, ProvingTransaction,
            WaitingForBroadcasterResponse,
        };
        use wallet_ops::SubmissionRouteState::{Ready, Reconnecting};
        // A confirmed setup is recorded for the network's accepted delegate.
        let delegate = self.root.upgrade().and_then(|root| {
            let chains = &root.read(cx).effective_chain_configs;
            Some(
                chains
                    .get(SWAP_ORIGIN)?
                    .accepted_executor_profile()?
                    .delegate(),
            )
        });
        let bridged = matches!(
            setup,
            Setup::TwoAccounts | Setup::TwoWaiting | Setup::DestinationFailed
        );
        let delivery = bridged.then_some(PRIVATE_DELIVERY);
        let operation = ExecutorOperationId::random().ok();
        let destination_operation = ExecutorOperationId::random().ok();
        let staged = || {
            let (operation, destination_operation) = (operation?, destination_operation?);
            // Each account's setup: whether it was sent, and whether it is confirmed.
            let (origin, destination) = match setup {
                Setup::Running
                | Setup::Waiting
                | Setup::Reconnecting
                | Setup::TwoWaiting
                | Setup::Failed
                | Setup::Retired
                | Setup::Replaced
                | Setup::FeeToken => ((false, false), (false, false)),
                Setup::BroadcasterError | Setup::Timeout | Setup::Restart | Setup::TwoAccounts => {
                    ((true, false), (false, false))
                }
                Setup::Ready => ((true, true), (false, false)),
                Setup::DestinationFailed => ((true, true), (true, false)),
            };
            let mut approval = staged_setup_approval(delivery)?;
            if setup == Setup::FeeToken {
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: USDC,
                    amount: U256::from(SETUP_APPROVED_FEE),
                });
            }
            if setup == Setup::Retired {
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: WETH,
                    amount: U256::from(SETUP_RETIRED_APPROVED_FEE),
                });
            }
            // Both accounts' uses started when the accounts were reserved.
            let swap_use = SwapUseId::first(operation);
            let started = now_unix().saturating_sub(240);
            let record = staged_setup_account(
                operation,
                (4, SWAP_ACCOUNT, delegate?),
                origin,
                &serde_json::json!({
                    "version": 2, "purpose_summary": "Private swap",
                    "retired": setup == Setup::Retired, "active_swap_use": swap_use,
                    "swap_uses": [{
                        "id": swap_use, "started_at": started, "fresh": true,
                        "role": { "Source": {
                            "approval": approval,
                            "destination_operation": delivery.map(|_| destination_operation),
                        }},
                    }],
                }),
            )?;
            let destination = match delivery {
                Some(delivery) => Some(staged_setup_account(
                    destination_operation,
                    (12, delivery.receiver, Address::repeat_byte(0x5d)),
                    destination,
                    &serde_json::json!({
                        "version": 2, "active_swap_use": swap_use,
                        "swap_uses": [{
                            "id": swap_use, "started_at": started, "fresh": true,
                            "role": { "Destination": {
                                "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                                "destination_token": delivery.destination_token,
                            }},
                        }],
                    }),
                )?),
                None => None,
            };
            Some((record, destination))
        };
        // A replaced account's swap is the one Stealth accounts lists in this mode.
        let staged = match setup {
            Setup::Replaced => replaced_accounts().map(|(swap, _)| (swap.clone(), None)),
            _ => staged(),
        };
        let Some((record, destination)) = staged else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        let operation = record.operation();
        let refused = PublicBroadcasterResultKind::Failed {
            error: "fee too low".to_owned(),
        };
        let submitted = PublicBroadcasterResultKind::Submitted {
            tx_hash: keccak256("setup"),
        };
        // What the setup job ended with. A failure its stages show isn't the swap's error too.
        let failed = "Fixture: the setup fee is above the limit approved with the swap.";
        // A setup that waits for a broadcaster's answer over `route`, published 27 seconds ago.
        let waiting = |route| {
            running_setup_progress(WaitingForBroadcasterResponse, route).waiting_since(
                std::rc::Rc::new(std::cell::Cell::new(
                    std::time::Instant::now().checked_sub(std::time::Duration::from_secs(27)),
                )),
            )
        };
        let (origin, destination_progress, error) = match setup {
            Setup::Running | Setup::Replaced => (
                Some(staged_setup_progress(ProvingTransaction, None)),
                None,
                None,
            ),
            Setup::Waiting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 3,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                None,
                None,
            ),
            Setup::Reconnecting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Reconnecting,
                    peers: 0,
                    recoveries_used: 1,
                    publishes: 3,
                })),
                None,
                None,
            ),
            Setup::TwoWaiting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 3,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 2,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                None,
            ),
            Setup::Failed => (
                Some(staged_setup_progress(
                    EstimatingBroadcasterFee,
                    Some(Err(failed.to_owned())),
                )),
                None,
                None,
            ),
            // The stage says what a setup refused for its fee token says there.
            Setup::FeeToken => {
                let root = self.root.upgrade();
                let changed = crate::root::private_swap::setup_fee_token_changed(
                    SWAP_ORIGIN,
                    root.as_ref()
                        .map(|root| &root.read(cx).effective_token_registry),
                    (USDC, U256::from(SETUP_APPROVED_FEE)),
                    WETH,
                    "Retry setup",
                );
                (
                    Some(staged_setup_progress(
                        EstimatingBroadcasterFee,
                        Some(Err(changed)),
                    )),
                    None,
                    None,
                )
            }
            Setup::BroadcasterError => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&refused)),
                )),
                None,
                None,
            ),
            Setup::Timeout => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
                )),
                None,
                None,
            ),
            // A retired account is told by the swap's stage, with no error beside it.
            Setup::Restart | Setup::Ready | Setup::Retired => (None, None, None),
            Setup::TwoAccounts => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&submitted)),
                )),
                Some(staged_setup_progress(GeneratingPoiProofs, None)),
                None,
            ),
            Setup::DestinationFailed => (
                None,
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&refused)),
                )),
                None,
            ),
        };
        let tracking = self.tracking.entry(operation).or_default();
        *tracking.setup_progress_mut(SetupSide::Origin) = origin;
        *tracking.setup_progress_mut(SetupSide::Destination) = destination_progress;
        tracking.error = error;
        // The review that approved the setup has the wallet place the order after it.
        tracking.auto_place = setup != Setup::Ready;
        // The view tells a running setup by its job, so one that never answers stands in.
        if matches!(
            setup,
            Setup::Running
                | Setup::Waiting
                | Setup::Reconnecting
                | Setup::Replaced
                | Setup::TwoAccounts
                | Setup::TwoWaiting
        ) {
            self.job = Some(SwapJob {
                operation,
                kind: SwapJobKind::Setup,
                abort: self
                    .runtime
                    .spawn(std::future::pending::<()>())
                    .abort_handle(),
            });
        }
        if let Some(destination) = destination {
            self.destinations.insert(
                SwapIdentity {
                    operation,
                    swap_use: SwapUseId::first(operation),
                },
                Some(destination),
            );
        }
        self.records = vec![record];
        self.show_detail(operation, window, cx);
        if setup == Setup::FeeToken
            && let (Some(delegate), Some(root)) = (delegate, self.root.upgrade())
        {
            // ETH at $3,000 and USDC at $1: a token's rate is its base units for one ETH.
            {
                let cache = &root.read(cx).public_broadcaster_anchor_cache;
                cache.store_native_usd_rate(SWAP_ORIGIN, U256::from(3_000_000_000_u64), 18);
                cache.store_rate(SWAP_ORIGIN, USDC, U256::from(3_000_000_000_u64));
                cache.store_rate(SWAP_ORIGIN, WETH, U256::from(1_000_000_000_000_000_000_u64));
            }
            let estimate = ExecutorRecoveryFeeEstimate::for_ui_fixture(
                SWAP_ORIGIN,
                WETH,
                delegate,
                U256::from(SETUP_RETRY_FEE),
            );
            self.open_ui_fixture_setup_retry_review(operation, &estimate, window, cx);
        }
    }

    /// In a `reported:` mode, show the detail of the synthesized swap with what this session
    /// would hold of its reported fill: the orderbook's report with the bought amount and the
    /// trade block, and what that block gave once read: the trade, a private swap's shield,
    /// and for a Bridge swap the hand-off, with the provider's report. A private Bridge swap
    /// also has its destination stealth account's record, and its report holds what the fill
    /// block on the destination network showed.
    fn open_ui_fixture_reported(
        &mut self,
        reported: Reported,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = staged_reported(reported) else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        let operation = record.operation();
        let Some(order) = record.swap().and_then(|swap| swap.orders().last()) else {
            return;
        };
        let uid = order.uid();
        if reported.private() {
            let swap_use = order
                .use_id()
                .unwrap_or_else(|| SwapUseId::first(operation));
            let destination = ExecutorOperationId::random().ok().and_then(|destination| {
                staged_setup_account(
                    destination,
                    (12, SETUP_DESTINATION_ACCOUNT, Address::repeat_byte(0x5d)),
                    (true, true),
                    &serde_json::json!({
                        "version": 2, "active_swap_use": swap_use,
                        "swap_uses": [{
                            "id": swap_use, "started_at": now_unix().saturating_sub(240),
                            "fresh": true,
                            "role": { "Destination": {
                                "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                                "destination_token": PRIVATE_DELIVERY.destination_token,
                            }},
                        }],
                    }),
                )
            });
            self.destinations.insert(
                SwapIdentity {
                    operation,
                    swap_use,
                },
                destination,
            );
        }
        let tracking = self.tracking.entry(operation).or_default();
        tracking.order_hints.insert(
            uid,
            SwapOrderHint {
                report: CowOrderStatusReport {
                    status: CowOrderStatusHint::Fulfilled,
                    executed_buy_amount: Some(U256::from(SWAP_BOUGHT)),
                },
                trade_block: Some(REPORTED_TRADE_BLOCK),
            },
        );
        let block = BlockNumHash::new(REPORTED_TRADE_BLOCK, keccak256("settlement"));
        let bridged = reported != Reported::Fill;
        let delivered = U256::from(RETRY.1);
        tracking.early_hints.insert(
            uid,
            SwapEarlyHint {
                trade_block: Some(REPORTED_TRADE_BLOCK),
                block_read: true,
                settlement: Some(SwapSettlementHint {
                    block,
                    bought: U256::from(SWAP_BOUGHT),
                    shielded: (!bridged).then_some(U256::from(REPORTED_SHIELDED)),
                    // The settlement transaction runs the post-hook's shield.
                    shield_transaction: (!bridged).then(|| keccak256("settlement transaction")),
                    handoff: bridged.then_some(SwapHandoffHint {
                        block,
                        deposit_id: Some(U256::from(3_481_207_u32)),
                        sent: U256::from(RETRY.0),
                    }),
                }),
                report: reported.delivered().then_some(SwapBridgeReport::Delivered {
                    amount_out: Some(delivered),
                    seen: match reported {
                        Reported::Shielded => {
                            Some(SwapDeliverySeen::Shielded { amount: delivered })
                        }
                        Reported::Held => Some(SwapDeliverySeen::Held { amount: delivered }),
                        Reported::Fill | Reported::Handoff | Reported::Delivered => None,
                    },
                    transaction: Some(keccak256("fill")),
                }),
                reads_fill: reported.private(),
            },
        );
        self.records = vec![record];
        self.show_detail(operation, window, cx);
    }

    /// In a `cancelled:` mode, stage account #5 and the destination account of its cancelled
    /// swap, then open the private form of a new swap that sells WETH. Its Source picker lists
    /// #5 when local admission offers it.
    fn open_ui_fixture_cancelled(
        &mut self,
        cancelled: Cancelled,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Local admission offers a source only for the network's accepted delegate.
        let delegate = self.root.upgrade().and_then(|root| {
            let chains = &root.read(cx).effective_chain_configs;
            Some(
                chains
                    .get(SWAP_ORIGIN)?
                    .accepted_executor_profile()?
                    .delegate(),
            )
        });
        let Some((record, swap, destination)) =
            delegate.and_then(|delegate| staged_cancelled(cancelled, delegate))
        else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        self.destinations.insert(swap, Some(destination));
        // The form lists its Source accounts from the records as it opens.
        self.records = vec![record];
        self.open_ui_fixture_new_swap(WETH, USDC, window, cx);
    }

    /// In a `retry-review`, `quote-error`, `split:`, speed-up, `setup:`, `reported:` or
    /// `cancelled:` mode, show the synthesized private swap in place of the Public account form
    /// that was asked for. `true` when such a mode is on.
    pub(in crate::root::private_swap) fn open_ui_fixture_swap(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        // The detail's stage, or none for a mode that opens the form.
        let detail = match mode() {
            Some(
                Mode::RetryReview
                | Mode::QuoteError
                | Mode::NativeBuy
                | Mode::SpeedUp(SpeedUp::Review)
                | Mode::Setup(_)
                | Mode::SetupUnderOrder(_)
                | Mode::Reported(_)
                | Mode::Cancelled(_)
                | Mode::NonEvm(..),
            ) => None,
            Some(Mode::Split(split)) => Some((Some(split), None)),
            Some(Mode::SpeedUp(speed_up)) => Some((None, Some(speed_up))),
            _ => return false,
        };
        if self.origin_chain_id != SWAP_ORIGIN {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture's private swap is on Ethereum: select it first");
            return true;
        }
        if let Some(Mode::Setup(setup)) = mode() {
            self.open_ui_fixture_setup(setup, window, cx);
            return true;
        }
        if let Some(Mode::SetupUnderOrder(under_order)) = mode() {
            let Some(record) = staged_setup_under_order(under_order == SetupUnderOrder::Expired)
            else {
                tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
                return true;
            };
            let operation = record.operation();
            self.records = vec![record];
            self.show_detail(operation, window, cx);
            return true;
        }
        if let Some(Mode::Reported(reported)) = mode() {
            self.open_ui_fixture_reported(reported, window, cx);
            return true;
        }
        if let Some(Mode::Cancelled(cancelled)) = mode() {
            self.open_ui_fixture_cancelled(cancelled, window, cx);
            return true;
        }
        if let Some(Mode::NonEvm(state, network)) = mode()
            && state != NonEvm::Review
        {
            let Some(record) = staged_non_evm(Some(state), network) else {
                tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
                return true;
            };
            let operation = record.operation();
            self.records = vec![record];
            self.show_detail(operation, window, cx);
            return true;
        }
        if matches!(mode(), Some(Mode::QuoteError)) {
            self.open_ui_fixture_quote_error(window, cx);
            return true;
        }
        if matches!(mode(), Some(Mode::NativeBuy)) {
            self.open_ui_fixture_new_swap(USDC, WETH, window, cx);
            return true;
        }
        let Some((split, speed_up)) = detail else {
            self.open_ui_fixture_retry_review(window, cx);
            return true;
        };
        let Some(record) = staged_swap(split, speed_up) else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its swap record");
            return true;
        };
        let operation = record.operation();
        // What an explicit check found in the stealth account: the refunded deposit.
        if let Some(refund) = record
            .swap()
            .and_then(|swap| swap.orders().first())
            .and_then(|order| order.observations().bridge_refund)
        {
            self.tracking.entry(operation).or_default().stealth_balance =
                Some((U256::from(FIRST_ORDER.0), refund.block));
        }
        self.records = vec![record];
        self.show_detail(operation, window, cx);
        true
    }
}
