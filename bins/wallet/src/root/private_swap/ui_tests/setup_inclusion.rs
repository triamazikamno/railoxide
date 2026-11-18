use super::*;

#[gpui::test]
fn setup_confirmation_tracks_local_inclusion_without_advancing_the_swap(cx: &mut TestAppContext) {
    use crate::root::utxo::UtxoFinalityContext;
    use alloy::primitives::B256;
    with_swap_view(cx, |_, _, executors, operation, _, _| {
        let record = pending_setup(executors, operation);
        let transaction = B256::repeat_byte(80);
        let record = executors
            .record_submission(operation, record.issued()[0].hash(), transaction)
            .unwrap();
        let mut utxo = wallet_ops::UtxoOutput {
            tree: 0,
            position: 0,
            token: String::new(),
            value: "1".into(),
            commitment_kind: "Transact".into(),
            activity_classification: "Private Output".into(),
            blocked_shield_rescue: None,
            commitment: String::new(),
            npk: String::new(),
            blinded_commitment: String::new(),
            poi_statuses: BTreeMap::new(),
            ppoi_state: wallet_ops::UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: B256::repeat_byte(90).to_string(),
            source_block_number: 90,
            source_block_timestamp: 0,
            is_spent: false,
            pending_new: false,
            pending_spent: true,
            local_pending_spent: false,
            spent_tx_hash: Some(transaction.to_string()),
            spent_block_number: Some(100),
            spent_block_timestamp: None,
        };
        // The spend is already visible in private sync, but not yet safe.
        for (head, expected) in [
            (100, "Confirming (0/12 blocks)"),
            (105, "Confirming (5/12 blocks)"),
            (112, "Verifying setup…"),
        ] {
            assert_eq!(
                model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo))
                    .and_then(|confirmation| confirmation.detail(UtxoFinalityContext::new(
                        Some(head),
                        Some(head - 12),
                        Some(12),
                    )))
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(
                swap_stage(&record, None, false, None),
                SwapStage::SetupPending
            );
        }
        // If private sync rolls the inclusion back, an unrelated transaction must
        // not leave the setup showing a stale confirmation count.
        utxo.spent_tx_hash = None;
        utxo.spent_block_number = None;
        let finality = UtxoFinalityContext::new(Some(105), Some(93), Some(12));
        assert!(model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo)).is_none());
        // Received change can provide the same hint even if the spent note is absent.
        utxo.source_tx_hash = transaction.to_string();
        utxo.source_block_number = 100;
        assert_eq!(
            model::swap_setup_confirmation(&record, &[utxo])
                .and_then(|confirmation| confirmation.detail(finality))
                .as_deref(),
            Some("Confirming (5/12 blocks)")
        );
    });
}

/// One setup observation of `operation`'s account: pending at the confirmed block, and with
/// `included` the latest block a read there showed the setup included at, the nonce after it.
fn setup_read(operation: ExecutorOperationId, included: Option<u64>) -> ObservationResult {
    ObservationResult {
        operation,
        range_end: None,
        outcome: Ok(Some(wallet_ops::SwapSetupObservation {
            status: wallet_ops::SwapSetupStatus::Pending,
            inclusion: included
                .map(|block| wallet_ops::SwapSetupInclusion::for_tests(block, U256::ONE)),
        })),
        destination: None,
        route: Arc::default(),
    }
}

/// One spendable note of `token` holding `amount`, from a transaction that isn't a setup's.
fn valid_note(token: Address, amount: u64) -> (Address, U256, B256, wallet_ops::UtxoPpoiState) {
    (
        token,
        U256::from(amount),
        B256::repeat_byte(6),
        wallet_ops::UtxoPpoiState::Valid,
    )
}

/// A second swap on another account, whose setup is confirmed, selling `Address::repeat_byte(2)`
/// with [`test_approval`]'s terms and placed by itself once it may be.
fn second_ready_swap(
    swaps: &Entity<PrivateSwapsView>,
    executors: &ExecutorStore,
    delegate: Address,
    cx: &mut gpui::VisualTestContext,
) -> ExecutorOperationId {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    let operation = ExecutorOperationId::random().unwrap();
    executors
        .reserve_with_swap_approval(
            operation,
            delegate,
            Some("Private swap"),
            &[
                wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
            ],
            Some(test_approval()),
            None,
        )
        .unwrap();
    executors
        .bind_address(operation, Address::repeat_byte(0x13))
        .unwrap();
    record_setup(
        executors,
        operation,
        delegate,
        B256::repeat_byte(0x14),
        Vec::new(),
    );
    confirm_setup(
        executors,
        operation,
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
    );
    cx.update(|_, cx| {
        swaps.update(cx, |swaps, _| {
            swaps.reload_records();
            swaps.tracking.entry(operation).or_default().auto_place = true;
        });
    });
    operation
}

/// An order of Address 1 for Address 2, back to the private balance, placed on the swap's own
/// setup while a read at the latest block showed it included: the setup at nonce 0 stays
/// unresolved, the pre-hook takes nonce 1 and the post-hook nonce 2, and both name the record's
/// confirmed observation. The orderbook accepted it, and it expires at `valid_to`.
fn included_order(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    valid_to: u32,
) -> broadcaster_core::contracts::cow::OrderUid {
    use alloy::primitives::Bytes;
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapAttempt, SwapProof, SwapRecipient, SwapSubmissionStatus,
        SwapTerms,
    };
    let setup = pending_setup(executors, operation);
    executors
        .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
        .unwrap();
    let observed = setup.nonce_observation().unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16198, "commitment": "0x2"
    }))
    .unwrap();
    let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), valid_to);
    let hook = |nonce: u64, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            setup.delegate(),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
        )
    };
    executors
        .record_swap_attempt_on_included_setup(
            operation,
            SwapAttempt {
                use_id: SwapUseId::first(operation),
                terms: SwapTerms::new(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    SwapRecipient::new(U256::ONE, [7; 32]),
                    setup.issued()[0].hash(),
                ),
                proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                uid,
                submission: None,
                delivery: SwapDelivery::Reshield,
                bounds: test_approval().bounds,
                invalidates: None,
                pre_hook: hook(1, 7, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                post_hook: Some(hook(2, 8, ExecutorPayloadPurpose::SwapPostHook, Vec::new())),
                bridge: None,
                speed_up: None,
            },
            U256::ONE,
        )
        .unwrap();
    executors
        .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
        .unwrap();
    uid
}

/// A swap whose setup a read at the latest block shows included is ready for its order, though
/// the record keeps the setup unconfirmed. An observation pass that reads it so places the
/// approved order, also for a setup whose broadcaster gave no transaction hash, and the detail
/// shows the order's steps with the setup's confirmation beside the stealth account until the
/// setup is confirmed. A private Bridge swap whose own setup is included still waits for its
/// destination account's setup.
#[gpui::test]
fn an_observation_pass_that_reads_the_setup_included_places_its_order(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let id = operation.opaque_id();
        let setup_row = |row: &str| format!("submission-step-swap-setup-{id}-origin-{row}");
        let order_row = |index: usize| format!("submission-step-swap-step-{id}-{index}");
        let shown = |cx: &mut gpui::VisualTestContext, selector: String| {
            cx.debug_bounds(selector.leak()).is_some()
        };
        // No transaction hash is recorded for the setup.
        let delegate = pending_setup(executors, operation).delegate();
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        // A private Bridge swap on another account, with no destination account set up.
        let bridge = ExecutorOperationId::random().unwrap();
        let mut approval = test_approval();
        approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: Address::repeat_byte(0x52),
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery {
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            }),
        });
        executors
            .reserve_with_swap_approval(
                bridge,
                delegate,
                Some("Private swap"),
                &[],
                Some(approval),
                None,
            )
            .unwrap();
        executors
            .bind_address(bridge, Address::repeat_byte(0x23))
            .unwrap();
        record_setup(
            executors,
            bridge,
            delegate,
            B256::repeat_byte(0x24),
            Vec::new(),
        );
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        owner.swap_notes_for_tests(&[valid_note(Address::repeat_byte(1), 1_000)]);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().auto_place = true;
                swaps.tracking.entry(bridge).or_default().auto_place = true;
                // The Bridge swap's own setup is included, and its destination isn't set up.
                swaps.apply_observations(vec![setup_read(bridge, Some(20))], window, cx);
                assert!(swaps.approved_use(swaps.record(bridge).unwrap()).is_none());
                // A pass that doesn't read the setup included places nothing.
                swaps.apply_observations(vec![setup_read(operation, None)], window, cx);
                assert!(swaps.job.is_none());
                swaps.apply_observations(vec![setup_read(operation, Some(20))], window, cx);
                let job = swaps.job.take().expect("the order is placed at inclusion");
                assert_eq!((job.operation, job.kind), (operation, SwapJobKind::Requote));
                job.abort.abort();
                swaps.job_revision += 1;
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::SetupPending);
                // The review's first step is marked done.
                assert!(swaps.approved_use(record).is_some());
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(shown(cx, order_row(1)) && !shown(cx, setup_row("sent")));
        assert!(cx.debug_bounds("swap-detail-account").is_some());
        assert!(cx.debug_bounds("swap-detail-account-setup").is_some());

        // The setup is confirmed: the account no longer shows its confirmation.
        confirm_setup(
            executors,
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, _| swaps.reload_records());
            window.draw(cx).clear(cx);
        });
        assert!(shown(cx, order_row(1)));
        assert!(cx.debug_bounds("swap-detail-account").is_some());
        assert!(cx.debug_bounds("swap-detail-account-setup").is_none());
    });
}

/// An included setup can place its order before finality. If the terms changed, its form
/// must review the order rather than silently return or offer to send the setup again.
#[gpui::test]
fn an_included_setup_can_review_changed_order_terms(cx: &mut TestAppContext) {
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.apply_observations(vec![setup_read(operation, Some(20))], window, cx);
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::SetupPending);
                assert!(swaps.approved_use(record).is_some());
                let approval = record.swap_approval().unwrap().clone();
                swaps.open_setup_retry(operation, SetupSide::Origin, window, cx);
                assert!(
                    swaps.setup_retry.is_none(),
                    "an included setup isn't sent again"
                );
                swaps.apply_approved_quote(
                    operation,
                    SwapUseId::first(operation),
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates)),
                    },
                    window,
                    cx,
                );
                let form = swaps
                    .form
                    .as_ref()
                    .expect("changed terms open the order form");
                assert_eq!(form.operation(), Some(operation));
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert_eq!(
                    swaps.reapproval,
                    Some((operation, SwapReviewChange::QuoteDeviates))
                );
                assert!(matches!(
                    swaps.form_quote_terms(form, cx).unwrap().executor,
                    QuoteExecutor::Order { operation: shown, reuse: false } if shown == operation
                ));
            });
        });
    });
}

/// When the approved spend fits only with the note the setup spent, as with a single note
/// that paid the setup's fee in the sell token, the swap sends no quote at inclusion and keeps
/// waiting to be placed by itself, while another ready swap is placed. It goes on waiting when
/// the record confirms the setup before private sync shows the change, and when the setup's
/// nonce is settled with the change awaiting its POI, which the owner's notes show before the
/// Private tab's do. Once the change is spendable the order is placed.
#[gpui::test]
fn automatic_placement_waits_for_the_setups_change(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::UtxoPpoiState;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let sell = Address::repeat_byte(1);
        let other = Address::repeat_byte(2);
        let transaction = B256::repeat_byte(80);
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        let held = [valid_note(sell, 1_000), valid_note(other, 1_000)];
        owner.swap_notes_for_tests(&held);
        let notes = ExecutorOwner::swap_note_identities_for_tests(&held);
        let delegate = executors
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .delegate();
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        // The setup paid its fee with the only note of the sell token.
        record_setup(
            executors,
            operation,
            delegate,
            B256::repeat_byte(4),
            vec![notes[0].clone()],
        );
        executors
            .record_submission(operation, B256::repeat_byte(4), transaction)
            .unwrap();
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let second = second_ready_swap(swaps, executors, delegate, cx);
        let job = |swaps: &mut PrivateSwapsView| {
            swaps.job.take().map(|job| {
                job.abort.abort();
                swaps.job_revision += 1;
                (job.operation, job.kind)
            })
        };
        let waiting = |swaps: &PrivateSwapsView| {
            let tracking = &swaps.tracking[&operation];
            tracking.auto_place && tracking.waits_for_change
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.tracking.entry(operation).or_default().auto_place = true;
                swaps.apply_observations(vec![setup_read(operation, Some(20))], window, cx);
                // The included swap waits, and the other one is placed.
                assert_eq!(job(swaps), Some((second, SwapJobKind::Requote)));
                assert!(waiting(swaps));
                swaps.tracking.entry(second).or_default().auto_place = false;
            });
        });

        // The record confirms the setup before private sync shows the spend or the change.
        confirm_setup(
            executors,
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        let pass = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.continue_approved_swaps(window, cx);
                    job(swaps)
                })
            })
        };
        assert_eq!(pass(cx), None);
        swaps.read_with(cx, |swaps, _| assert!(waiting(swaps)));

        // Private sync settles the setup's nonce. Its change is in the owner's notes, which the
        // Private tab doesn't list yet, and its POI is pending.
        executors.record_synced_for_tests(operation, 12).unwrap();
        let change = |state| (sell, U256::from(900), transaction, state);
        owner.swap_notes_for_tests(&[
            valid_note(other, 1_000),
            change(UtxoPpoiState::ProofSubmitted),
        ]);
        assert_eq!(pass(cx), None);
        swaps.read_with(cx, |swaps, _| assert!(waiting(swaps)));
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| swaps.show_detail(operation, window, cx));
            window.draw(cx).clear(cx);
        });
        let order_step = format!("submission-step-swap-step-{}-1", operation.opaque_id());
        assert!(cx.debug_bounds(order_step.leak()).is_some());
        cx.update(|window, cx| {
            window.close_dialog(cx);
            window.draw(cx).clear(cx);
        });

        // The change's POI is valid: the order is placed.
        owner.swap_notes_for_tests(&[valid_note(other, 1_000), change(UtxoPpoiState::Valid)]);
        assert_eq!(pass(cx), Some((operation, SwapJobKind::Requote)));
    });
}

/// A setup whose broadcaster gave no transaction hash can't name its change, so automatic
/// placement waits only until its nonce is settled. A spend that still doesn't fit is then
/// attempted, which reports the shortfall as any attempt does.
#[gpui::test]
fn a_setup_without_a_transaction_waits_only_for_settlement(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let sell = Address::repeat_byte(1);
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        let held = [valid_note(sell, 1_000)];
        owner.swap_notes_for_tests(&held);
        let notes = ExecutorOwner::swap_note_identities_for_tests(&held);
        let delegate = executors
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .delegate();
        executors
            .bind_address(operation, Address::repeat_byte(3))
            .unwrap();
        record_setup(
            executors,
            operation,
            delegate,
            B256::repeat_byte(4),
            vec![notes[0].clone()],
        );
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let pass = |cx: &mut gpui::VisualTestContext, read: Option<ObservationResult>| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    match read {
                        Some(read) => {
                            swaps.apply_observations(vec![read], window, cx);
                        }
                        None => swaps.continue_approved_swaps(window, cx),
                    }
                    swaps.job.take().map(|job| {
                        job.abort.abort();
                        swaps.job_revision += 1;
                        job.kind
                    })
                })
            })
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.tracking.entry(operation).or_default().auto_place = true;
            });
        });
        assert_eq!(pass(cx, Some(setup_read(operation, Some(20)))), None);
        confirm_setup(
            executors,
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        assert_eq!(pass(cx, None), None);
        // Settled, with the spent note gone and a change no recorded transaction names.
        executors.record_synced_for_tests(operation, 12).unwrap();
        owner.swap_notes_for_tests(&[(
            sell,
            U256::from(900),
            B256::repeat_byte(81),
            wallet_ops::UtxoPpoiState::Missing,
        )]);
        assert_eq!(pass(cx, None), Some(SwapJobKind::Requote));
    });
}

/// Private sync's evidence of the swap's own setup, under an order placed on it while it was
/// included, starts an observation pass at once. Each pass reads that setup beside the order,
/// also after the order ended.
#[gpui::test]
fn a_setup_under_an_order_is_read_each_pass_and_woken_by_private_sync(cx: &mut TestAppContext) {
    use wallet_ops::vault::{SwapObservation, SwapOrderObservations};
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let uid = included_order(executors, operation, u32::MAX);
        let transaction = B256::repeat_byte(80);
        let record = executors
            .record_submission(operation, B256::repeat_byte(4), transaction)
            .unwrap();
        let mut utxo = wallet_ops::UtxoOutput {
            tree: 0,
            position: 0,
            token: String::new(),
            value: "1".into(),
            commitment_kind: "Transact".into(),
            activity_classification: "Private Output".into(),
            blocked_shield_rescue: None,
            commitment: String::new(),
            npk: String::new(),
            blinded_commitment: String::new(),
            poi_statuses: BTreeMap::new(),
            ppoi_state: wallet_ops::UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: B256::repeat_byte(90).to_string(),
            source_block_number: 90,
            source_block_timestamp: 0,
            is_spent: false,
            pending_new: false,
            pending_spent: true,
            local_pending_spent: false,
            spent_tx_hash: None,
            spent_block_number: None,
            spent_block_timestamp: None,
        };
        let records = std::slice::from_ref(&record);
        assert!(synced_setup_inclusions(records, std::slice::from_ref(&utxo)).is_empty());
        utxo.spent_tx_hash = Some(transaction.to_string());
        utxo.spent_block_number = Some(100);
        assert!(synced_setup_inclusions(records, &[utxo]).contains(&operation));

        let setup_page = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.next_observations(cx).is_some_and(|(_, _, pages)| {
                        pages.iter().any(|page| {
                            page.operation == operation
                                && page.range.is_none()
                                && page.settlement.is_none()
                        })
                    })
                })
            })
        };
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
        });
        assert!(setup_page(cx), "the open order's setup is read");
        let seen = SwapObservation {
            block: record.nonce_observation().unwrap().block(),
            transaction_hash: None,
        };
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    expired: Some(seen),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(setup_page(cx), "the ended order's setup is still read");
    });
}

/// While a setup below an open order is unconfirmed, Cancel order… is disabled with its reason,
/// whether a read at the latest block shows the setup included or not. Speed up… follows that
/// read as signing does, and an active cancellation still hides it. Once the setup is
/// confirmed, Cancel order… is offered as before.
#[gpui::test]
fn cancel_waits_for_the_setup_below_the_order_and_speed_up_follows_its_inclusion(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let now = u32::try_from(now_unix()).unwrap();
        included_order(executors, operation, now + 600);
        // The detail with this session's latest read: its Speed up and whether Cancel order…
        // started anything when clicked.
        let detail = |cx: &mut gpui::VisualTestContext, included: bool, cancelling: bool| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let tracking = swaps.tracking.entry(operation).or_default();
                    tracking.inclusion =
                        included.then(|| wallet_ops::SwapSetupInclusion::for_tests(20, U256::ONE));
                    tracking.cancelling = cancelling;
                    tracking.error = None;
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let speed_up = cx.debug_bounds("swap-progress-speed-up").is_some();
            let cancel = cx.debug_bounds("swap-progress-cancel-order").unwrap();
            cx.simulate_click(cancel.center(), gpui::Modifiers::none());
            let cancelled = swaps.read_with(cx, |swaps, _| {
                swaps.job.is_some() || swaps.tracking[&operation].error.is_some()
            });
            (speed_up, cancelled)
        };
        assert_eq!(detail(cx, true, false), (true, false), "included");
        assert_eq!(
            detail(cx, false, false),
            (false, false),
            "no longer included"
        );
        assert_eq!(detail(cx, true, true), (false, false), "cancelling");

        confirm_setup(
            executors,
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
        );
        let (_, cancelled) = detail(cx, false, false);
        assert!(
            cancelled,
            "Cancel order… is offered once the setup is confirmed"
        );
    });
}

/// A setup that stops being included before its order is signed returns the swap to its setup:
/// a pass that no longer reads it included, or a signing that finds it gone, which leaves no
/// error and keeps the order to be placed by itself.
#[gpui::test]
fn a_setup_no_longer_included_before_signing_returns_the_swap_to_its_setup(
    cx: &mut TestAppContext,
) {
    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let id = operation.opaque_id();
        let setup_row = format!("submission-step-swap-setup-{id}-origin-sent").leak();
        pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.apply_observations(vec![setup_read(operation, Some(20))], window, cx);
                assert!(
                    swaps
                        .approved_use(swaps.record(operation).unwrap())
                        .is_some()
                );
                swaps.apply_observations(vec![setup_read(operation, None)], window, cx);
                assert!(
                    swaps
                        .approved_use(swaps.record(operation).unwrap())
                        .is_none()
                );
            });
        });
        draw(cx);
        assert!(cx.debug_bounds(setup_row).is_some());

        // Included again, and gone when the order is signed.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.apply_observations(vec![setup_read(operation, Some(21))], window, cx);
                // The order is being placed.
                swaps.tracking.entry(operation).or_default().pending_order =
                    Some(PendingSwapOrder {
                        previous_order: None,
                        sell: Address::repeat_byte(1),
                        buy: Address::repeat_byte(2),
                        delivery: SwapDelivery::Reshield,
                        amount: U256::from(100),
                        private_minimum: U256::from(98),
                        slippage_bps: 50,
                        gas_share_bps: wallet_ops::cow::GAS_SHARE_BALANCED_BPS,
                        valid_for: DRAFT_VALIDITY,
                        reuse_account: false,
                        swap_use: SwapUseId::first(operation),
                        started_at: now_unix(),
                        speeds_up: false,
                    });
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    async {
                        Err::<(), _>(eyre::Report::from(
                            wallet_ops::vault::SwapAccountRefusal::SetupUnconfirmed,
                        ))
                    },
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
            });
        });
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, _| {
            let tracking = &swaps.tracking[&operation];
            assert!(swaps.job.is_none() && swaps.error.is_none());
            assert!(tracking.error.is_none() && tracking.inclusion.is_none());
            assert!(tracking.auto_place && tracking.pending_order.is_none());
        });
        draw(cx);
        assert!(cx.debug_bounds(setup_row).is_some());
    });
}

/// Under an order placed on the swap's own setup while it was included, open or ended, the
/// stealth account says the setup is no longer included once neither private sync nor the
/// latest read shows it, and offers Retry setup…, which is disabled while the latest read
/// shows it included again. The retry saves a raised setup fee limit, in any token, with an
/// approval that binds no accounts.
#[gpui::test]
fn a_setup_no_longer_included_under_its_order_offers_its_retry(cx: &mut TestAppContext) {
    use wallet_ops::vault::{SwapObservation, SwapOrderObservations};
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let uid = included_order(executors, operation, u32::MAX);
        let retry = |cx: &mut gpui::VisualTestContext, included: bool| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.tracking.entry(operation).or_default().inclusion =
                        included.then(|| wallet_ops::SwapSetupInclusion::for_tests(20, U256::ONE));
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let fact = cx.debug_bounds("swap-detail-account-setup").is_some();
            let availability = swaps.read_with(cx, |swaps, cx| {
                swaps.setup_retry_availability(
                    swaps.record(operation).unwrap(),
                    SetupSide::Origin,
                    cx,
                )
            });
            (fact, availability)
        };
        assert_eq!(
            retry(cx, true),
            (
                true,
                Some(Err(gpui::SharedString::from(SETUP_ALREADY_INCLUDED)))
            )
        );
        assert_eq!(retry(cx, false), (true, Some(Ok(()))));
        let button = cx.debug_bounds("swap-setup-retry-origin").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        swaps.read_with(cx, |swaps, _| {
            assert!(
                swaps
                    .setup_retry
                    .as_ref()
                    .is_some_and(|retry| retry.operation == operation),
                "the retry's dialog opened"
            );
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, _| swaps.setup_retry = None);
            window.close_all_dialogs(cx);
        });

        // The retry saves a raised setup fee limit beside the order, and the approval still
        // binds no accounts.
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        let persisted = || {
            executors
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .and_then(|record| record.swap_approval().cloned())
                .unwrap()
        };
        let mut raised = persisted();
        raised.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
            token: STUB_USDC,
            amount: U256::from(2),
        });
        owner
            .bind_swap_use_accounts(operation, SwapUseId::first(operation), Some(raised.clone()))
            .unwrap();
        assert_eq!(persisted(), raised);

        // The order expired, and the setup never returned.
        let seen = SwapObservation {
            block: swaps.read_with(cx, |swaps, _| {
                swaps
                    .record(operation)
                    .unwrap()
                    .nonce_observation()
                    .unwrap()
                    .block()
            }),
            transaction_hash: None,
        };
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    expired: Some(seen),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(retry(cx, false), (true, Some(Ok(()))));

        // Beside the ended order, the retry may also pay its fee in another token.
        raised.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
            token: STUB_USDT,
            amount: U256::from(3),
        });
        owner
            .bind_swap_use_accounts(operation, SwapUseId::first(operation), Some(raised.clone()))
            .unwrap();
        assert_eq!(persisted(), raised);
    });
}

#[gpui::test]
fn stale_setup_read_is_dropped_and_a_failed_read_records_its_error(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                // A setup's read pages through no history, so it has no range.
                let result = |outcome| {
                    vec![ObservationResult {
                        operation,
                        range_end: None,
                        outcome,
                        destination: None,
                        route: Arc::default(),
                    }]
                };
                // Either account's background read can lose a race with confirmation.
                // Its stale result must neither show a review error nor advance tracking.
                for destination in [None, Some(operation)] {
                    let error = eyre::Report::from(wallet_ops::ExecutorRecordChanged)
                        .wrap_err("checking setup");
                    let mut stale = result(Err(error));
                    stale[0].destination = destination;
                    assert_eq!(
                        swaps.apply_observations(stale, window, cx),
                        [] as [ExecutorOperationId; 0]
                    );
                    assert!(swaps.tracking.get(&operation).is_none_or(|tracking| {
                        tracking.error.is_none()
                            && tracking.setup.is_none()
                            && tracking.destination_setup.is_none()
                    }));
                }
                // No broadcaster answered the setup this session sent, which its stages keep
                // telling. A read that fails afterwards is told as the swap's error.
                drop(swaps.start_setup_progress(
                    operation,
                    SetupSide::Origin,
                    setup_broadcaster(),
                    cx,
                ));
                swaps.finish_setup_progress(
                    operation,
                    SetupSide::Origin,
                    Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
                );
                assert!(
                    swaps
                        .apply_observations(
                            result(Err(eyre::eyre!("RPC unavailable"))),
                            window,
                            cx,
                        )
                        .is_empty(),
                    "failed reads wait before retrying"
                );
                let error = swaps.tracking.get(&operation).unwrap().error.as_deref();
                assert!(error.is_some_and(|error| error.contains("RPC unavailable")));
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        // The detail shows both: the read's error, and the unanswered setup on its own stage.
        assert!(cx.debug_bounds("swap-progress-error").is_some());
        let unanswered = format!(
            "submission-step-swap-setup-{}-origin-wait-response",
            operation.opaque_id()
        );
        assert!(cx.debug_bounds(unanswered.leak()).is_some());
    });
}
