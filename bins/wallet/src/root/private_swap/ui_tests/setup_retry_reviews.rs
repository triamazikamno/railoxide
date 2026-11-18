use super::*;

/// Cancel preparation… is confirmed after the running setup job replaced the swap's used
/// account, with the job's report of that `followed` by the view or still unread. Either way
/// the confirmation cancels the swap where it is by then.
fn cancel_preparation_after_replacement(cx: &mut TestAppContext, followed: bool) {
    use wallet_ops::vault::SwapUseSide;
    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let swap_use = SwapUseId::first(operation);
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        executors
            .record_swap_approval(operation, swap_use, test_approval())
            .unwrap();
        // This session sends the setup, and the user asks to cancel while it runs.
        let job = runtime.spawn(std::future::pending::<()>());
        let (revision, (replacements, _following)) = cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: job.abort_handle(),
                });
                swaps.tracking.entry(operation).or_default().auto_place = true;
                swaps.show_detail(operation, window, cx);
                (
                    swaps.job_revision,
                    PrivateSwapsView::follow_replacements(cx),
                )
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let cancel = cx.debug_bounds("swap-progress-cancel-preparation").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());

        // Before the user answers, the job finds the account used and replaces it.
        let replacement = ExecutorOperationId::random().unwrap();
        executors.retire(operation).unwrap();
        executors
            .replace_swap_use_account(operation, swap_use, SwapUseSide::Source, replacement)
            .unwrap();
        executors
            .bind_address(replacement, Address::repeat_byte(4))
            .unwrap();
        if followed {
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
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        // The job is stopped and its completion no longer applies.
        assert!(
            runtime
                .block_on(job)
                .is_err_and(|error| error.is_cancelled())
        );
        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.job.is_none() && swaps.job_revision != revision);
            // Nothing continues the swap, which the view holds under its new account alone.
            assert!(!swaps.tracking.contains_key(&operation));
            assert!(!swaps.tracking[&replacement].auto_place);
            assert_eq!(
                swaps.dialog.as_ref().map(|dialog| dialog.view),
                Some(SwapDialogView::Detail(replacement))
            );
            let cancelled = swaps
                .cancelled
                .as_ref()
                .expect("the detail reports the cancellation");
            assert_eq!(cancelled.operation, replacement);
            let [source] = cancelled.accounts.as_slice() else {
                panic!("the swap's own account is reported");
            };
            assert_eq!(
                source.account.as_ref().map(|account| account.address),
                Some(Address::repeat_byte(4))
            );
        });
        // The use is stopped on the replacement, the only account that holds it.
        let records = executors.records().unwrap();
        let claimed = |operation| {
            records
                .iter()
                .find(|record| record.operation() == operation)
                .and_then(|record| record.swap_use(swap_use))
                .map(wallet_ops::vault::SwapUseRecord::is_stopped)
        };
        assert_eq!(
            (claimed(operation), claimed(replacement)),
            (None, Some(true))
        );
    });
}

#[gpui::test]
fn cancelling_preparation_follows_a_replacement_the_view_has_followed(cx: &mut TestAppContext) {
    cancel_preparation_after_replacement(cx, true);
}

#[gpui::test]
fn cancelling_preparation_follows_a_replacement_the_view_has_not_read(cx: &mut TestAppContext) {
    cancel_preparation_after_replacement(cx, false);
}

/// The fixture swap stopped at its retired account with an approval saved, and the fee estimate
/// of a new account's setup with the broadcaster that offers it.
fn retired_swap_with_estimate(
    root: &Entity<WalletRoot>,
    swaps: &Entity<PrivateSwapsView>,
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    runtime: &tokio::runtime::Runtime,
    cx: &gpui::VisualTestContext,
) -> (PublicBroadcasterCandidate, ExecutorRecoveryFeeEstimate) {
    use wallet_ops::vault::SwapApprovalTokens;
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let mut approval = test_approval();
    approval.tokens = Some(SwapApprovalTokens {
        sell: STUB_USDC,
        buy: STUB_USDT,
    });
    executors
        .record_swap_approval(operation, SwapUseId::first(operation), approval)
        .unwrap();
    executors.retire(operation).unwrap();
    let (session, owner) = swaps.read_with(cx, |swaps, _| {
        (
            Arc::clone(swaps.private_session().unwrap()),
            Arc::clone(swaps.private_owner().unwrap()),
        )
    });
    owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
    let offer = setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
    let estimate = runtime
        .block_on(Box::pin(owner.estimate_swap_setup_fee(
            &session,
            None,
            offer.clone(),
            &operation_client(root, runtime, cx),
        )))
        .unwrap();
    (offer, estimate)
}

/// A swap that stopped at a retired account and has a saved approval goes on with a new
/// account. Retry with new account… opens the retry dialog in place of the swap's form, also
/// in a wallet restarted there, which holds the swap's records alone and starts the
/// broadcaster controls from their defaults. The review keeps the saved pair, amount, minimum
/// and delivery, names a new account, and reserves nothing. Approving it reserves one new
/// account, which takes the swap use, and leaves the retired one with none.
#[gpui::test]
fn retry_with_new_account_keeps_the_saved_terms_and_reserves_one_account(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            let swap_use = SwapUseId::first(operation);
            let (offer, estimate) =
                retired_swap_with_estimate(root, swaps, executors, operation, runtime, cx);
            let waku = idle_waku(root, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // Nothing of this session is held about the swap, as after a restart.
                    swaps.tracking.remove(&operation);
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let resume = cx.debug_bounds("swap-progress-continue").unwrap();
            cx.simulate_click(resume.center(), gpui::Modifiers::none());
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert!(cx.debug_bounds("swap-setup-retry-new-account").is_some());
            let saved = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    assert!(swaps.form.is_none(), "the swap isn't reviewed again");
                    let retry = swaps.setup_retry.as_mut().expect("the retry's dialog");
                    assert_eq!(
                        (retry.operation, retry.side),
                        (operation, SetupSide::Origin)
                    );
                    let route = &mut retry.route;
                    assert!(route.selected.is_none() && !route.fee_token_chosen);
                    // The route as the dialog's controls leave it once a broadcaster offers.
                    route.fee_token = Some(STUB_USDC);
                    route.fee_token_chosen = true;
                    route.candidates = vec![offer.clone()];
                    route.selected = Some(offer.railgun_address.clone());
                    route.estimate = Some(estimate);
                    swaps.quote_setup_retry(Some(waku), window, cx);
                    swaps
                        .record(operation)
                        .and_then(|record| record.swap_approval())
                        .unwrap()
                        .clone()
                })
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            let retry = swaps.read_with(cx, |swaps, cx| {
                let pending = swaps.pending_authorization.as_ref().unwrap_or_else(|| {
                    panic!(
                        "{:?}",
                        swaps
                            .setup_retry
                            .as_ref()
                            .and_then(|retry| retry.error.clone())
                    )
                });
                let SwapAction::SetupRetry(retry) = &pending.action else {
                    panic!("the retry's review");
                };
                assert!(retry.replaces);
                let mut approval = saved.clone();
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: STUB_USDC,
                    amount: retry.maximum_private_fee,
                });
                assert_eq!(
                    retry.approval.as_ref(),
                    Some(&approval),
                    "the pair, amount, minimum and delivery are the saved ones"
                );
                let rows = swaps.setup_retry_summary(retry, cx).rows_for_test();
                assert!(
                    rows.contains(&("Stealth account".to_owned(), "New account".to_owned())),
                    "{rows:?}"
                );
                (**retry).clone()
            });
            assert_eq!(
                executors.records().unwrap().len(),
                1,
                "a review that isn't approved reserves nothing"
            );

            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // This password doesn't open the fixture's vault, so the new account can't
                    // be prepared. wallet-ops covers the setup that follows a preparation.
                    let authorization = DesktopPrivateSpendAuthorization::VaultPassword(
                        zeroize::Zeroizing::new("approved".to_owned()),
                    );
                    swaps.submit_setup_retry(retry, authorization, window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            let records = executors.records().unwrap();
            let (retired, replacements): (Vec<_>, Vec<_>) = records
                .iter()
                .partition(|record| record.operation() == operation);
            assert!(retired[0].is_retired() && retired[0].swap_uses().is_empty());
            let [replacement] = replacements.as_slice() else {
                panic!("one new account is reserved: {}", replacements.len());
            };
            let claimed = replacement.swap_use(swap_use).expect("the swap's use");
            assert_eq!(claimed.approval(), Some(&saved));
            swaps.read_with(cx, |swaps, _| {
                let shown = swaps
                    .shown_swaps()
                    .map(|(record, _)| record.operation())
                    .collect::<Vec<_>>();
                assert_eq!(shown, [replacement.operation()]);
            });
        },
    );
}

/// A hardware wallet's device approves one account's action. So its retry with a new account
/// reserves the replacement before any authorization is requested, and the review's hardware
/// action names the replacement's operation. The swap then sits on the new account, whose
/// setup an ordinary retry sends, also when the review is cancelled.
#[gpui::test]
fn a_hardware_retry_with_new_account_reserves_it_before_its_review(cx: &mut TestAppContext) {
    use wallet_ops::vault::WalletSource;
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            let swap_use = SwapUseId::first(operation);
            let (_, estimate) =
                retired_swap_with_estimate(root, swaps, executors, operation, runtime, cx);
            let waku = idle_waku(root, cx);
            root.update(cx, |root, _| {
                let wallet_id = root
                    .selected_wallet_id
                    .clone()
                    .expect("the fixture's wallet");
                root.wallet_options = vec![crate::root::vault::WalletOption {
                    wallet_id,
                    source: WalletSource::LedgerDerived,
                }];
            });
            let (replacement, command) = swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let replacement = swaps
                    .reserve_reviewed_replacement(operation, SetupSide::Origin, cx)
                    .unwrap();
                assert!(swaps.pending_authorization.is_none());
                let reserved = swaps.record(replacement).expect("the reserved replacement");
                assert!(!reserved.is_retired() && reserved.swap_use(swap_use).is_some());
                assert_ne!(swaps.stage(reserved), SwapStage::SetupRetired);
                let retry = swaps
                    .setup_retry_approval(replacement, SetupSide::Origin, &estimate, waku, cx)
                    .unwrap();
                assert!(!retry.replaces, "the retry's job replaces nothing more");
                let command = SwapAuthorization {
                    session: Arc::clone(swaps.private_session().unwrap()),
                    destination_session: None,
                    action: SwapAction::SetupRetry(Box::new(retry)),
                    destination: None,
                };
                (replacement, command)
            });
            assert_ne!(replacement, operation);
            assert!(matches!(
                command.hardware_executor_action(),
                HardwareExecutorAction::Execute(named) if named == replacement
            ));
            let records = executors.records().unwrap();
            let retired = records
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            assert!(retired.is_retired() && retired.swap_uses().is_empty());
        },
    );
}

/// Retry setup… sits on the stage a setup failed at: the response stage after a broadcaster
/// didn't answer. A setup whose nonce is recorded as consumed waits for its verification, so
/// its action opens nothing.
#[gpui::test]
fn retry_setup_sits_on_the_failed_stage_while_the_setup_can_be_sent_again(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let show = |cx: &mut gpui::VisualTestContext, operation| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };

        // No broadcaster answered the setup this session sent.
        pending_setup(executors, operation);
        swaps.update(cx, |swaps, cx| {
            drop(swaps.start_setup_progress(operation, SetupSide::Origin, setup_broadcaster(), cx));
            swaps.finish_setup_progress(
                operation,
                SetupSide::Origin,
                Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
            );
        });
        show(cx, operation);
        assert!(cx.debug_bounds("swap-progress-continue").is_none());
        let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        let unanswered = format!(
            "submission-step-swap-setup-{}-origin-wait-response",
            operation.opaque_id()
        );
        let unanswered = cx.debug_bounds(unanswered.leak()).unwrap();
        assert!(unanswered.contains(&retry.center()));

        // Another swap's setup ran for a delegate this wallet doesn't accept, so its consumed
        // nonce doesn't make the account ready and the setup still waits for its verification.
        let included = ExecutorOperationId::random().unwrap();
        let delegate = Address::repeat_byte(0x99);
        executors
            .reserve_with_swap_approval(
                included,
                delegate,
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
                Some(test_approval()),
                None,
            )
            .unwrap();
        executors
            .bind_address(included, Address::repeat_byte(0x35))
            .unwrap();
        record_setup(
            executors,
            included,
            delegate,
            B256::repeat_byte(5),
            Vec::new(),
        );
        confirm_setup(
            executors,
            included,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        show(cx, included);
        swaps.read_with(cx, |swaps, cx| {
            let record = swaps.record(included).unwrap();
            assert_eq!(swaps.stage(record), SwapStage::SetupPending);
            assert_eq!(
                swaps.setup_retry_availability(record, SetupSide::Origin, cx),
                Some(Err(SETUP_ALREADY_INCLUDED.into()))
            );
        });
        let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        swaps.read_with(cx, |swaps, _| assert!(swaps.setup_retry.is_none()));
    });
}

/// A setup's own failure is told once. The swap's detail tells it on the stage the setup ended
/// at, and not again as the swap's error. Without stages the detail can show, it is the swap's
/// error: before the swap is saved, and for a setup that held no stages. The swap's form, open
/// while another setup is sent, tells a failure its stages keep.
#[gpui::test]
fn a_setup_failure_is_told_on_its_stage_or_as_the_swaps_error(cx: &mut TestAppContext) {
    use crate::root::private_swap::setup_progress::SetupProgressEnd;
    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let show = |cx: &mut gpui::VisualTestContext, operation| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };
        let fail = |cx: &mut gpui::VisualTestContext, operation, stages: bool, message: &str| {
            swaps.update(cx, |swaps, cx| {
                // A job starts without an error of an earlier one.
                swaps.error = None;
                swaps.tracking.entry(operation).or_default().error = None;
                if stages {
                    // The setup reported its first stage before its job failed.
                    swaps
                        .start_setup_progress(operation, SetupSide::Origin, setup_broadcaster(), cx)
                        .0
                        .send(TransactionGenerationStage::SelectingPrivateNotes)
                        .unwrap();
                }
                swaps.fail_setup_job(operation, &eyre::eyre!("{message}"), cx);
            });
        };
        let error = |cx: &mut gpui::VisualTestContext, operation| {
            swaps.read_with(cx, |swaps, _| swaps.tracking[&operation].error.clone())
        };

        // The swap's stealth account couldn't be reserved, so no record holds the swap.
        let unsaved = ExecutorOperationId::random().unwrap();
        fail(cx, unsaved, true, "not reserved");
        assert_eq!(error(cx, unsaved).as_deref(), Some("not reserved"));
        show(cx, unsaved);
        assert!(cx.debug_bounds("swap-progress-error").is_some());

        // The swap is saved, and the setup that failed held no stages.
        pending_setup(executors, operation);
        swaps.update(cx, |swaps, _| swaps.reload_records());
        fail(cx, operation, false, "not prepared");
        assert_eq!(error(cx, operation).as_deref(), Some("not prepared"));
        show(cx, operation);
        assert!(cx.debug_bounds("swap-progress-error").is_some());

        // The setup failed at a stage the detail shows.
        fail(cx, operation, true, "not proved");
        assert_eq!(error(cx, operation), None);
        swaps.read_with(cx, |swaps, _| {
            let progress = swaps.tracking[&operation].setup_progress(SetupSide::Origin);
            assert_eq!(
                progress.and_then(SetupProgress::end),
                Some(SetupProgressEnd::Failed)
            );
            assert_eq!(
                progress.and_then(SetupProgress::problem).as_deref(),
                Some("not proved")
            );
        });
        show(cx, operation);
        assert!(cx.debug_bounds("swap-progress-error").is_none());
        let failed = format!(
            "submission-step-swap-setup-{}-origin-select-notes",
            operation.opaque_id()
        );
        assert!(cx.debug_bounds(failed.leak()).is_some());

        // The form of a swap whose setup is being sent says what those stages keep.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_existing_form(operation, window, cx);
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: runtime.spawn(std::future::pending::<()>()).abort_handle(),
                });
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-setup-error").is_some());
        swaps.update(cx, |swaps, _| {
            if let Some(job) = swaps.job.take() {
                job.abort.abort();
            }
        });
    });
}

/// A setup sent again by itself keeps the terms saved with the swap. Its fee limit is the fresh
/// estimate's: one above the approved limit is named in the review, which carries it with the
/// swap's approval, where nothing else changes. The limit is saved with the swap's account
/// bindings once the account is prepared, so a setup that can't be prepared saves nothing, and
/// its failure, which comes before the setup's first stage, is told as the swap's error.
#[gpui::test]
fn a_setup_retry_keeps_the_saved_terms_and_saves_a_raised_fee_limit(cx: &mut TestAppContext) {
    use wallet_ops::vault::SwapApprovalTokens;
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            executors
                .bind_address(operation, Address::repeat_byte(3))
                .unwrap();
            let mut saved = test_approval();
            saved.tokens = Some(SwapApprovalTokens {
                sell: STUB_USDC,
                buy: STUB_USDT,
            });
            let approved = SwapSetupFeeLimit {
                token: STUB_USDC,
                amount: U256::ONE,
            };
            saved.bounds.source_setup_fee = Some(approved);
            executors
                .record_swap_approval(operation, SwapUseId::first(operation), saved)
                .unwrap();
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
            let offer = setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
            let estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    Some(operation),
                    offer,
                    &operation_client(root, runtime, cx),
                )))
                .unwrap();
            let waku = idle_waku(root, cx);
            let retry = swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let retry = swaps
                    .setup_retry_approval(operation, SetupSide::Origin, &estimate, waku, cx)
                    .unwrap();
                let raised = retry.maximum_private_fee;
                assert!(raised > U256::ONE);
                let mut approval = swaps
                    .record(operation)
                    .and_then(|record| record.swap_approval())
                    .unwrap()
                    .clone();
                assert_eq!(approval.bounds.source_setup_fee, Some(approved));
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: STUB_USDC,
                    amount: raised,
                });
                assert_eq!(
                    retry.approval.as_ref(),
                    Some(&approval),
                    "the pair, amount, minimum and delivery are the saved ones"
                );
                let warnings = swaps.setup_retry_summary(&retry, cx).warnings_for_test();
                assert!(
                    matches!(
                        warnings.as_slice(),
                        [warning] if warning.starts_with("The setup fee on Ethereum is above the")
                            && warning.ends_with("Approving this review saves the new limit.")
                    ),
                    "{warnings:?}"
                );
                retry
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // This password doesn't open the fixture's vault, so the account can't be
                    // prepared. wallet-ops covers the save that follows a real preparation.
                    let authorization = DesktopPrivateSpendAuthorization::VaultPassword(
                        zeroize::Zeroizing::new("approved".to_owned()),
                    );
                    swaps.submit_setup_retry(retry, authorization, window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            let records = executors.records().unwrap();
            let saved = records
                .iter()
                .find(|record| record.operation() == operation)
                .and_then(|record| record.swap_approval())
                .unwrap();
            assert_eq!(saved.bounds.source_setup_fee, Some(approved));
            swaps.read_with(cx, |swaps, _| {
                let tracking = &swaps.tracking[&operation];
                // Preparation failed before the setup's first stage, so no stage holds the
                // failure and the swap's own error tells it.
                assert!(tracking.setup_progress(SetupSide::Origin).is_none());
                assert!(tracking.error.is_some());
            });
        },
    );
}

/// The retry's review tells how its fee limit differs from the one approved with the swap. A
/// higher limit in the approved token is named with the increase in the token and in USD.
/// Another fee token is named with both limits, each in its own token, and with the change in
/// USD alone. Without a price the review names both limits and no change.
#[gpui::test]
fn a_setup_retry_review_tells_a_changed_fee_limit_and_fee_token(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, _, operation, _, cx| {
        root.update(cx, |root, _| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
        });
        let waku = idle_waku(root, cx);
        // The retry pays up to 0.52 USDC.
        let candidate = setup_offer(root, 1, STUB_USDC, U256::ONE, cx);
        let shown = |approved: SwapSetupFeeLimit, cx: &mut gpui::VisualTestContext| {
            let retry = SetupRetry {
                operation,
                side: SetupSide::Origin,
                swap_use: SwapUseId::first(operation),
                chain_id: 1,
                account: operation,
                candidate: candidate.clone(),
                maximum_private_fee: U256::from(520_000),
                waku: waku.clone(),
                approved_fee: Some(approved),
                approval: None,
                replaces: false,
            };
            swaps.read_with(cx, |swaps, cx| {
                let summary = swaps.setup_retry_summary(&retry, cx);
                (
                    summary.row_delta_for_test("Pay now"),
                    summary.warnings_for_test(),
                )
            })
        };
        let usdc = SwapSetupFeeLimit {
            token: STUB_USDC,
            amount: U256::from(400_000),
        };
        let weth = SwapSetupFeeLimit {
            token: STUB_WETH,
            amount: U256::from(200_000_000_000_000_u64),
        };

        // No price is cached yet.
        assert_eq!(
            shown(weth, cx),
            (
                None,
                vec![
                    "The fee token changed from WETH to USDC. You approved up to 0.0002 WETH; this setup pays up to 0.52 USDC. Approving this review saves the new limit."
                        .to_owned()
                ]
            )
        );

        // ETH at $3,000, and USDC at $1.
        root.read_with(cx, |root, _| {
            let cache = &root.public_broadcaster_anchor_cache;
            cache.store_native_usd_rate(1, U256::from(3_000_000_000_u64), 18);
            cache.store_rate(1, STUB_USDC, U256::from(3_000_000_000_u64));
            cache.store_rate(1, STUB_WETH, U256::from(10).pow(U256::from(18)));
        });
        assert_eq!(
            shown(usdc, cx),
            (
                Some(("+0.12 USDC · +$0.12".to_owned(), true)),
                vec![
                    "The setup fee on Ethereum is above the 0.4 USDC you approved. Approving this review saves the new limit."
                        .to_owned()
                ]
            )
        );
        assert_eq!(
            shown(weth, cx),
            (
                Some(("−$0.08".to_owned(), false)),
                vec![
                    "The fee token changed from WETH to USDC. You approved up to 0.0002 WETH ($0.60); this setup pays up to 0.52 USDC ($0.52). Approving this review saves the new limit."
                        .to_owned()
                ]
            )
        );
    });
}

/// A reviewed setup fee limit is within the approved one only in the approved token. Amounts
/// in different tokens are never compared, and without an approved limit there is no change.
#[test]
fn setup_fee_limit_changes_compare_amounts_only_in_one_token() {
    let limit = |token: u8, amount: u64| SwapSetupFeeLimit {
        token: Address::repeat_byte(token),
        amount: U256::from(amount),
    };
    for (approved, reviewed, change) in [
        (limit(1, 5), limit(1, 5), SetupFeeLimitChange::Within),
        (limit(1, 5), limit(1, 4), SetupFeeLimitChange::Within),
        (limit(1, 5), limit(1, 6), SetupFeeLimitChange::Higher),
        (limit(1, 5), limit(2, 4), SetupFeeLimitChange::AnotherToken),
    ] {
        assert_eq!(
            setup_fee_limit_change(Some(approved), reviewed),
            Some(change),
            "{approved:?} {reviewed:?}"
        );
    }
    assert_eq!(setup_fee_limit_change(None, limit(1, 5)), None);
}

/// A swap that sets up its own stealth account saves that setup's fee token and limit with
/// its approval, also when it isn't a private Bridge swap. A swap on a reused account sets
/// none up and saves none.
#[gpui::test]
fn a_swap_saves_the_fee_token_and_limit_of_its_own_setup(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            reusable_account(executors, operation);
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
            let offer = setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
            let estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    None,
                    offer.clone(),
                    &operation_client(root, runtime, cx),
                )))
                .unwrap();
            open_private_form(root, swaps, &stubs, runtime, cx);
            let review = ready_review(swaps, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    for reused in [false, true] {
                        swaps.select_form_account(reused.then_some(operation), window, cx);
                        // The route holds an estimate, also for an account that needs no setup.
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        form.route.estimate = Some(estimate.clone());
                        form.route.candidates = vec![offer.clone()];
                        let form = swaps.form.as_ref().unwrap();
                        let parts = swaps.setup_parts(form, &review, false).unwrap();
                        let bounds = &parts.approval.bounds;
                        assert_eq!(
                            (bounds.source_setup_fee, bounds.destination_setup_fee),
                            (
                                (!reused).then(|| SwapSetupFeeLimit {
                                    token: STUB_USDC,
                                    amount: default_public_broadcaster_fee_limit(
                                        estimate.fee_amount()
                                    ),
                                }),
                                None,
                            ),
                            "reused: {reused}"
                        );
                    }
                });
            });
        },
    );
}

/// A setup's retry opens from its row with the broadcaster controls of that account's network.
/// While no broadcaster can be paid there it says so, and Continue estimates and signs
/// nothing. With one, Continue estimates the fee for the broadcaster and fee token the dialog
/// holds, and the review that takes the dialog's place names them.
#[gpui::test]
fn a_setup_retry_reviews_the_broadcaster_and_fee_token_its_dialog_holds(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            executors
                .bind_address(operation, Address::repeat_byte(3))
                .unwrap();
            executors
                .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
                .unwrap();
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
            let offer = setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
            let estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    Some(operation),
                    offer.clone(),
                    &operation_client(root, runtime, cx),
                )))
                .unwrap();
            let waku = idle_waku(root, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let retry = cx.debug_bounds("swap-setup-retry-origin").unwrap();
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            cx.update(|window, cx| window.draw(cx).clear(cx));

            // The wallet's monitor holds no broadcaster offer.
            assert!(cx.debug_bounds("swap-setup-retry-unavailable").is_some());
            // A switch the user toggled holds the focus, and stops tracking it while the
            // estimate disables the dialog's controls.
            let switch = cx.debug_bounds("out-of-range").unwrap();
            cx.simulate_click(switch.center(), gpui::Modifiers::none());
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let proceed = cx.debug_bounds("swap-setup-retry-continue").unwrap();
            cx.simulate_click(proceed.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.quote_setup_retry(Some(waku.clone()), window, cx);
                    assert!(
                        swaps.job.is_none() && swaps.pending_authorization.is_none(),
                        "nothing is estimated or signed without a broadcaster"
                    );

                    // The route as the dialog's fee token selector and broadcaster picker
                    // leave it, with the estimate of the broadcaster chosen.
                    let route = &mut swaps.setup_retry.as_mut().unwrap().route;
                    route.fee_token = Some(STUB_USDC);
                    route.fee_token_chosen = true;
                    route.candidates = vec![offer.clone()];
                    route.selected = Some(offer.railgun_address.clone());
                    route.estimate = Some(estimate);
                    // The broadcaster picker reads the retry's route, on its network.
                    let picker = swaps.setup_picker_context().unwrap();
                    assert_eq!((picker.chain_id, picker.token), (1, STUB_USDC));
                    assert_eq!(picker.candidates.len(), 1);
                    swaps.quote_setup_retry(Some(waku), window, cx);
                    assert_eq!(
                        swaps.job.as_ref().map(|job| job.kind),
                        Some(SwapJobKind::SetupQuote)
                    );
                });
                // A frame while the estimate runs, with the dialog's controls disabled.
                window.draw(cx).clear(cx);
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            swaps.read_with(cx, |swaps, cx| {
                let pending = swaps.pending_authorization.as_ref().unwrap_or_else(|| {
                    panic!(
                        "{:?}",
                        swaps
                            .setup_retry
                            .as_ref()
                            .and_then(|retry| retry.error.clone())
                    )
                });
                let SwapAction::SetupRetry(retry) = &pending.action else {
                    panic!("the retry's review");
                };
                assert_eq!((retry.side, retry.account), (SetupSide::Origin, operation));
                assert_eq!(retry.candidate.railgun_address, offer.railgun_address);
                assert_eq!(retry.candidate.token, STUB_USDC);
                assert!(
                    swaps.setup_retry.is_none(),
                    "the review takes the dialog's place"
                );
                let paid = swaps
                    .setup_retry_summary(retry, cx)
                    .row_hint_for_test("Pay now")
                    .unwrap();
                assert!(
                    paid.contains(&broadcaster_candidate_label(&retry.candidate)),
                    "{paid}"
                );
            });
        },
    );
}

#[gpui::test]
fn dismissed_expired_swap_stays_dormant_after_restart(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation,
        SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause, SwapProof, SwapRecipient,
        SwapTerms,
    };
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        let setup_hash = setup.issued()[0].hash();
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors.record_account_read(operation, observed).unwrap();
        let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
            "tree": 4, "position": 16198, "commitment": "0x2"
        }))
        .unwrap();
        let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20);
        let hook = |nonce, hash, purpose, inputs| {
            IssuedExecutorPayload::new(
                U256::from(nonce),
                setup.delegate(),
                B256::repeat_byte(hash),
                purpose,
                ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
            )
        };
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
                    use_id: wallet_ops::vault::SwapUseId::first(operation),
                    terms: SwapTerms::new(
                        Address::repeat_byte(1),
                        Address::repeat_byte(2),
                        SwapRecipient::new(U256::ONE, [7; 32]),
                        setup_hash,
                    ),
                    proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                    uid,
                    submission: None,
                    delivery: SwapDelivery::Reshield,
                    bounds: test_approval().bounds,
                    invalidates: None,
                    pre_hook: hook(
                        1_u64,
                        7,
                        ExecutorPayloadPurpose::SwapPreHook,
                        vec![input.clone()],
                    ),
                    post_hook: Some(hook(
                        2_u64,
                        8,
                        ExecutorPayloadPurpose::SwapPostHook,
                        Vec::new(),
                    )),
                    bridge: None,
                    speed_up: None,
                },
            )
            .unwrap();
        let record = executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_dead: Some(SwapPreHookDeath {
                        cause: SwapPreHookDeathCause::Expired,
                        observation: SwapObservation {
                            block: observed.block(),
                            transaction_hash: None,
                        },
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!record.reserved_inputs().contains(&input));
        executors.set_hidden(operation, true).unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(record.nonce_observation(), Some(observed));
                assert!(!record.reserved_inputs().contains(&input));
                assert!(!swaps.has_shown_swaps());
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "completed swaps must not trigger account-specific RPC after restart"
                );
                // Removed from the Private tab, the swap stays in My orders.
                swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let row = cx
            .debug_bounds(format!("swap-order-row-{}", operation.opaque_id()).leak())
            .expect("a removed swap stays listed in My orders");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            assert_eq!(
                swaps.read(cx).dialog.as_ref().map(|dialog| dialog.view),
                Some(dialog::SwapDialogView::Detail(operation)),
                "selecting it opens its detail"
            );
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-progress-remove").is_none(),
            "an already removed swap isn't offered for removal again"
        );

        // Older records may have lost their nonce evidence during an interrupted check.
        // Keep those notes locked, but don't turn startup into a migration RPC sweep.
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(
                    swaps
                        .record(operation)
                        .unwrap()
                        .reserved_inputs()
                        .contains(&input)
                );
                assert!(swaps.next_observations(cx).is_none());
            });
        });

        let pending = ExecutorOperationId::random().unwrap();
        executors
            .reserve(pending, setup.delegate(), Some("Private swap"), &[])
            .unwrap();
        executors
            .bind_address(pending, Address::repeat_byte(9))
            .unwrap();
        let pending_nonce = ExecutorNonceObservation::new(observed.block(), U256::ZERO);
        executors
            .record_account_read(pending, pending_nonce)
            .unwrap();
        executors
            .record_issued(
                pending,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    setup.delegate(),
                    B256::repeat_byte(9),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        pending_nonce,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let (_, _, pages) = swaps.next_observations(cx).expect("resume pending setup");
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].operation, pending);
            });
        });
    });
}
