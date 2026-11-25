use super::*;

#[gpui::test]
fn pending_setup_can_retry_and_stop_without_losing_its_reservation(cx: &mut TestAppContext) {
    use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, NonEvmDelivery, SwapAccountChoice, SwapApprovalTokens,
        SwapPairClaim,
    };
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        // Hiding an account is only presentation; it must not stop its pending swap.
        executors.set_hidden(operation, true).unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // A setup unconfirmed for long is still read on every pass.
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(1_000);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(observed(swaps, cx));
            });
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(swaps.has_shown_swaps());
                assert!(!swaps.record(operation).unwrap().is_swap_setup_stopped());
                swaps.tracking.entry(operation).or_default().auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::SetupPending
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        // The swap's approval is saved, so its setup is sent again from the setup's own row,
        // which keeps the approval. The footer doesn't reopen the form for it.
        assert!(cx.debug_bounds("swap-progress-continue").is_none());
        let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let retry = swaps.setup_retry.as_ref().expect("the setup's retry");
                assert_eq!(
                    (retry.operation, retry.side),
                    (operation, SetupSide::Origin)
                );
                assert!(swaps.form.is_none());
                // Opening the retry neither stops the setup nor gives up its reservation.
                assert!(swaps.tracking.get(&operation).unwrap().auto_place);
                let preview = swaps
                    .private_owner()
                    .unwrap()
                    .swap_setup_preview(operation)
                    .unwrap();
                assert_eq!(preview.executor(), issued.address().unwrap());
                assert!(preview.delegated().is_none());
                // A local delivery may still be waiting when the user stops it.
                let join = runtime.spawn(std::future::pending::<()>());
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: join.abort_handle(),
                });
                swaps.show_detail(operation, window, cx);
                assert!(
                    swaps.setup_retry.is_none(),
                    "the retry goes with its dialog"
                );
            });
            window.draw(cx).clear(cx);
        });
        let stop = cx.debug_bounds("swap-progress-cancel-preparation").unwrap();
        cx.simulate_click(stop.center(), gpui::Modifiers::none());
        // Confirm the alert through its normal keyboard action.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            let record = swaps.record(operation).unwrap();
            assert!(record.is_swap_setup_stopped());
            assert!(!swaps.has_shown_swaps());
            assert!(swaps.job.is_none());
            assert!(!swaps.tracking.get(&operation).unwrap().auto_place);
            assert_eq!(record.reserved_inputs(), issued.reserved_inputs());
        });
        // Nothing waits on a stopped setup, so the swap view stops reading its account.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(!observed(swaps, cx));
            });
        });
        // Its detail keeps the setup's steps and offers no retry.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.cancelled = None;
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let confirmation = format!(
            "submission-step-swap-setup-{}-origin-confirm",
            operation.opaque_id()
        );
        assert!(cx.debug_bounds(confirmation.leak()).is_some());
        assert!(cx.debug_bounds("swap-setup-retry-origin").is_none());
        // Unhiding the account cannot undo the persisted stop, even if setup confirms later.
        executors.set_hidden(operation, false).unwrap();
        assert!(
            executors
                .records()
                .unwrap()
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap()
                .is_swap_setup_stopped()
        );
        assert!(
            executors
                .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
                .is_err()
        );
        let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
        let record = executors
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
            )
            .unwrap();
        let profile = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
        });
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        let setup = wallet_ops::swap_setup_status(&record, confirmed, &code, profile);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.setup = Some(setup);
                tracking.auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::Approved
                );
                assert!(!swaps.has_shown_swaps());
                swaps.continue_approved_swaps(window, cx);
                assert!(swaps.pending_authorization.is_none());
                assert!(swaps.job.is_none());
            });
        });

        // Nothing of the cancelled swap is unresolved.
        let first = SwapUseId::first(operation);
        let identity = model::SwapIdentity {
            operation,
            swap_use: first,
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(!swaps.cancelled_use(identity, cx).unwrap().needs_attention());
            });
        });
        // A new swap takes the account and is shown while it is prepared. It bridges to
        // Bitcoin, which its title names rather than the intermediate it buys here.
        let second = SwapUseId::random().unwrap();
        let mut later = test_approval();
        later.bounds.sell_amount = U256::from(200);
        later.tokens = Some(SwapApprovalTokens {
            sell: Address::repeat_byte(1),
            buy: Address::repeat_byte(2),
        });
        later.delivery = SwapDelivery::NonEvmBridge(NonEvmDelivery {
            network: NonEvmNetwork::Bitcoin,
            receiver: "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq".to_owned(),
            asset_id: "nep141:btc.omft.near".to_owned(),
            contract: None,
            symbol: "BTC".to_owned(),
            decimals: 8,
        });
        executors
            .claim_swap_pair(SwapPairClaim {
                id: second,
                source: SwapAccountChoice::Existing(operation),
                delegate: record.delegate(),
                purpose_summary: None,
                assets: Vec::new(),
                approval: later,
                destination: None,
            })
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(swaps.has_shown_swaps());
                assert_eq!(swaps.open_order_count(cx), 1);
            });
        });
        // Cancelled before its order too, the account holds only cancelled preparations: no
        // card stays on the Private tab, also after reopening the view, and My orders lists
        // both cancellations. The account isn't retired here, so its stage alone would still
        // show it.
        executors.cancel_swap_use(operation, second).unwrap();
        let only_cancelled = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            assert!(!swaps.record(operation).unwrap().is_retired());
            assert!(!swaps.has_shown_swaps());
            assert_eq!(swaps.open_order_count(cx), 0);
            assert_eq!(swaps.order_rows_for_test(cx).len(), 2);
            let title = swaps
                .cancelled_use(
                    model::SwapIdentity {
                        operation,
                        swap_use: second,
                    },
                    cx,
                )
                .unwrap()
                .title;
            assert!(title.ends_with("→ BTC on Bitcoin"), "{title}");
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                only_cancelled(swaps, cx);
            });
        });
        let swaps = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.clear_private_swaps(cx);
                root.ensure_private_swaps(window, cx);
                root.private_swaps_view().unwrap()
            })
        });
        cx.update(|_, cx| swaps.update(cx, |swaps, cx| only_cancelled(swaps, cx)));
        // The account then signs a recovery that is still unresolved. That later work is not
        // the first cancelled swap's attention.
        executors
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    record.delegate(),
                    B256::repeat_byte(5),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"recovery"),
                        ExecutorNonceObservation::new(confirmed, U256::ONE),
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(record.has_unresolved_issued_work());
                assert!(!swaps.cancelled_use(identity, cx).unwrap().needs_attention());
            });
        });
    });
}

#[gpui::test]
fn setup_whose_nonce_is_recorded_as_consumed_is_approved_and_needs_no_setup_page(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().setup =
                    Some(wallet_ops::SwapSetupStatus::Pending);
            });
        });
        // The account's nonce is read past the setup. The swap is ready to place its approved
        // order without another account read, also once that read's observation is
        // invalidated.
        let consumed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors.record_account_read(operation, consumed).unwrap();
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(record.nonce_observation().is_none());
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().setup,
                    Some(wallet_ops::SwapSetupStatus::Pending)
                );
                assert_eq!(swaps.stage(record), SwapStage::Approved);
                assert!(
                    !observed(swaps, cx),
                    "a setup recorded as resolved needs no setup page"
                );
            });
        });
    });
}

/// A swap that sets its stealth account up shows the setup until the account is ready, and
/// the order after that. Without this session's stages, as after a restart, one step stands
/// for the sending. While the setup's job runs, its six stages and the confirmation show, and
/// no step of the order. A ready swap lists the order's steps without the setup and names the
/// account among its facts. A retired account's swap shows its setup again. Only this
/// session's stages name the setup's broadcaster, which offers to become a favorite once it
/// submitted the setup.
#[gpui::test]
fn swap_detail_shows_the_setup_until_its_account_is_ready(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let id = operation.opaque_id();
        let setup_row = |row: &str| format!("submission-step-swap-setup-{id}-origin-{row}");
        let order_row = |index: usize| format!("submission-step-swap-step-{id}-{index}");
        let shown = |cx: &mut gpui::VisualTestContext, selector: String| {
            cx.debug_bounds(selector.leak()).is_some()
        };
        let show = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };

        // The setup was sent in an earlier session. Its retry sits on the row it waits at.
        show(cx);
        assert!(shown(cx, setup_row("sent")) && shown(cx, setup_row("confirm")));
        assert!(!shown(cx, setup_row("prove")) && !shown(cx, order_row(1)));
        let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        let waiting = cx.debug_bounds(setup_row("confirm").leak()).unwrap();
        assert!(waiting.contains(&retry.center()));
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-origin").is_none(),
            "a setup sent in an earlier session names no broadcaster"
        );

        // This session sends it again.
        let (stages, _routes) = swaps.update(cx, |swaps, cx| {
            swaps.job = Some(SwapJob {
                operation,
                kind: SwapJobKind::Setup,
                abort: runtime.spawn(std::future::pending::<()>()).abort_handle(),
            });
            swaps.start_setup_progress(operation, SetupSide::Origin, setup_broadcaster(), cx)
        });
        stages
            .send(TransactionGenerationStage::ProvingTransaction)
            .unwrap();
        show(cx);
        for row in [
            "select-notes",
            "prove",
            "estimate-fee",
            "poi-proofs",
            "publish",
            "wait-response",
            "confirm",
        ] {
            assert!(shown(cx, setup_row(row)), "{row}");
        }
        assert!(!shown(cx, setup_row("sent")) && !shown(cx, order_row(1)));
        assert!(
            cx.debug_bounds("swap-setup-retry-origin").is_none(),
            "a setup that is being sent has no retry"
        );
        assert!(cx.debug_bounds("swap-setup-broadcaster-origin").is_some());
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-favorite-origin")
                .is_none()
        );

        // Its broadcaster submitted it, so it can become a favorite.
        swaps.update(cx, |swaps, _| {
            assert!(swaps.finish_setup_progress(
                operation,
                SetupSide::Origin,
                Some(Ok(&PublicBroadcasterResultKind::Submitted {
                    tx_hash: B256::repeat_byte(1),
                })),
            ));
        });
        show(cx);
        let favorite = cx
            .debug_bounds("swap-setup-broadcaster-favorite-origin")
            .unwrap();
        cx.simulate_click(favorite.center(), gpui::Modifiers::none());
        show(cx);
        assert!(root.read_with(cx, |root, _| {
            root.is_favorite_broadcaster(SETUP_BROADCASTER)
        }));
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-favorite-origin")
                .is_none()
        );
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-favorited-origin")
                .is_some()
        );
        // The rest follows the records alone, as after a restart.
        swaps.update(cx, |swaps, _| {
            swaps.finish_setup_progress(operation, SetupSide::Origin, None);
        });

        // The setup is confirmed, so the order's steps follow.
        swaps.update(cx, |swaps, _| {
            if let Some(job) = swaps.job.take() {
                job.abort.abort();
            }
        });
        confirm_setup(
            executors,
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        show(cx);
        assert!(!shown(cx, order_row(0)) && shown(cx, order_row(1)));
        assert!(!shown(cx, setup_row("prove")) && !shown(cx, setup_row("confirm")));
        assert!(cx.debug_bounds("swap-detail-account").is_some());

        executors.retire(operation).unwrap();
        show(cx);
        assert!(shown(cx, setup_row("sent")) && !shown(cx, order_row(1)));
        assert!(cx.debug_bounds("swap-setup-retry-origin").is_none());
    });
}

/// A setup that waits for a broadcaster's answer is offered again once its request was
/// re-sent five times: Retry setup… sits on its waiting row, and stops that wait as the
/// timeout would, so the setup ends unanswered and can still be included, before the retry's
/// dialog opens. The swap still places its order once the setup is confirmed. From then on
/// its Broadcaster row also offers Ban, which bans that broadcaster and leaves the wait
/// running.
#[gpui::test]
fn a_setup_waiting_for_five_resends_offers_its_retry(cx: &mut TestAppContext) {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use crate::root::private_swap::setup_progress::{SetupProgress, SetupProgressEnd};

    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let waiting_since = Rc::new(Cell::new(
            Instant::now().checked_sub(Duration::from_secs(26)),
        ));
        let (_stages, routes) = cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: runtime.spawn(std::future::pending::<()>()).abort_handle(),
                });
                swaps.show_detail(operation, window, cx);
                let (stages, receiver) = tokio::sync::watch::channel(
                    TransactionGenerationStage::WaitingForBroadcasterResponse,
                );
                let (routes, route) =
                    tokio::sync::watch::channel(wallet_ops::SubmissionRouteProgress::default());
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.auto_place = true;
                *tracking.setup_progress_mut(SetupSide::Origin) = Some(
                    SetupProgress::running(setup_broadcaster(), receiver, route, None)
                        .waiting_since(Rc::clone(&waiting_since)),
                );
                (stages, routes)
            })
        });

        // Re-sent twice: the setup still waits, with no retry.
        routes.send_modify(|route| route.publishes = 3);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-setup-retry-origin").is_none());
        assert!(cx.debug_bounds("swap-setup-broadcaster-origin").is_some());
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-ban-origin")
                .is_none()
        );

        // Re-sent five times: its row offers the retry and Ban, while its job runs.
        routes.send_modify(|route| route.publishes = 6);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let ban = cx
            .debug_bounds("swap-setup-broadcaster-ban-origin")
            .unwrap();
        cx.simulate_click(ban.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(root.read_with(cx, |root, _| root.is_banned_broadcaster(SETUP_BROADCASTER)));
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-ban-origin")
                .is_none()
        );
        assert!(
            cx.debug_bounds("swap-setup-broadcaster-banned-origin")
                .is_some()
        );
        assert!(
            swaps.read_with(cx, |swaps, _| swaps.job.is_some()),
            "the ban left the wait running"
        );
        let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));

        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.job.is_none(), "the wait stopped");
            let tracking = &swaps.tracking[&operation];
            assert_eq!(
                tracking
                    .setup_progress(SetupSide::Origin)
                    .and_then(SetupProgress::end),
                Some(SetupProgressEnd::TimedOut)
            );
            assert!(tracking.auto_place);
            assert!(
                swaps
                    .setup_retry
                    .as_ref()
                    .is_some_and(|retry| retry.operation == operation),
                "the retry's dialog opened"
            );
        });
    });
}

/// A setup job that replaces the swap's used account keeps it one swap. The view moves the
/// swap when the job reports the replacement: its detail stays open on the new account with
/// the setup's stages, which the running setup keeps reporting into, and the swap keeps its
/// start time and terms. Nothing the view holds for the swap names the retired account: the
/// Private tab and My orders list the swap once, under its new account, and the retired
/// account is left to Stealth accounts as an account that holds no swap.
#[gpui::test]
fn a_replaced_account_keeps_the_swap_and_its_open_detail(cx: &mut TestAppContext) {
    use wallet_ops::vault::SwapUseSide;
    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let swap_use = SwapUseId::first(operation);
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        let started = executors
            .record_swap_approval(operation, swap_use, test_approval())
            .unwrap()
            .swap_use(swap_use)
            .unwrap()
            .started_at();
        // This session sends the setup, and the swap's detail shows it.
        let ((stages, _routes), (replacements, _following)) = cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: runtime.spawn(std::future::pending::<()>()).abort_handle(),
                });
                swaps.show_detail(operation, window, cx);
                (
                    swaps.start_setup_progress(
                        operation,
                        SetupSide::Origin,
                        setup_broadcaster(),
                        cx,
                    ),
                    PrivateSwapsView::follow_replacements(cx),
                )
            })
        });

        // The job finds the account used, retires it, replaces it and reports that.
        let replacement = ExecutorOperationId::random().unwrap();
        executors.retire(operation).unwrap();
        executors
            .replace_swap_use_account(operation, swap_use, SwapUseSide::Source, replacement)
            .unwrap();
        replacements
            .send(ReplacedSwapAccount {
                from: operation,
                swap: SwapAccountReplacement {
                    operation: replacement,
                    swap_use,
                    replacement,
                },
            })
            .unwrap();
        cx.run_until_parked();
        stages
            .send(TransactionGenerationStage::PublishingToBroadcaster)
            .unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        swaps.read_with(cx, |swaps, _| {
            assert_eq!(
                swaps.dialog.as_ref().map(|dialog| dialog.view),
                Some(SwapDialogView::Detail(replacement))
            );
            assert!(
                swaps
                    .job
                    .as_ref()
                    .is_some_and(|job| job.operation == replacement)
            );
            assert!(!swaps.tracking.contains_key(&operation));
            assert_eq!(
                swaps.tracking[&replacement]
                    .setup_progress(SetupSide::Origin)
                    .and_then(SetupProgress::stage),
                Some(TransactionGenerationStage::PublishingToBroadcaster)
            );
            // The Private tab lists one swap, with the start time and terms it had.
            let shown = swaps
                .shown_swaps()
                .map(|(record, _)| record.operation())
                .collect::<Vec<_>>();
            assert_eq!(shown, [replacement]);
            let claimed = swaps
                .record(replacement)
                .and_then(|record| record.swap_use(swap_use))
                .unwrap();
            assert_eq!(claimed.started_at(), started);
            assert_eq!(claimed.approval(), Some(&test_approval()));
        });
        let publishing = format!(
            "submission-step-swap-setup-{}-origin-publish",
            replacement.opaque_id()
        );
        assert!(
            cx.debug_bounds(publishing.leak()).is_some(),
            "the open detail shows the replacement's running setup"
        );
        swaps.update(cx, |swaps, _| {
            if let Some(job) = swaps.job.take() {
                job.abort.abort();
            }
        });

        // My orders lists the swap once, started when it first claimed an account.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert_eq!(
                    swaps.swap_started(swaps.record(replacement).unwrap(), None, cx),
                    started.map(|at| ("Started", at))
                );
                swaps.orders_filter = None;
                swaps.show_view(SwapDialogView::Orders, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let row = |operation: ExecutorOperationId, cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds(format!("swap-order-row-{}", operation.opaque_id()).leak())
                .is_some()
        };
        assert!(row(replacement, cx) && !row(operation, cx));
        // Stealth accounts lists every record, and tags the ones that are a swap's.
        let records = executors.records().unwrap();
        let listed = |operation| {
            records
                .iter()
                .find(|record| record.operation() == operation)
                .map(|record| (record.is_retired(), wallet_ops::is_swap_record(record)))
        };
        assert_eq!(listed(operation), Some((true, false)));
        assert_eq!(listed(replacement), Some((false, true)));
    });
}
