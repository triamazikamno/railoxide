use super::*;

/// Choose `account` in a stealth account select, as a click on its row does.
fn confirm_account(
    select: &Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>,
    account: Option<ExecutorOperationId>,
    window: &mut Window,
    cx: &mut App,
) {
    select.update(cx, |select, cx| {
        select.set_selected_value(&account, window, cx);
        cx.emit(SelectEvent::<SearchableVec<SwapAccountSelectItem>>::Confirm(Some(account)));
    });
}

/// Private balance on another network chooses a stealth account on each network by itself.
/// Polygon holds no private funds and no broadcaster offers, only a set-up account: its routes
/// stay selectable, a new account there leaves Review unavailable with the setup-funding
/// reason, and nothing selects the existing one. Once it holds funds, the picker says that the
/// setup fee is being checked, and then that it couldn't be, instead of asking for an existing
/// account. Choosing that account needs no setup there. A refused choice stays with its
/// reason, a token change keeps the choice and checks it again, a network change returns the
/// destination to a new account and keeps the source, and a new session's form has neither
/// choice. Any other delivery has one selector.
#[gpui::test]
fn private_bridge_accounts_are_chosen_independently(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon, destination) = reusable_polygon_account(root, cx);
            let chosen = |swaps: &PrivateSwapsView| {
                let form = swaps.form.as_ref().unwrap();
                (
                    form.operation,
                    form.destination_choice().map(|account| account.operation),
                )
            };
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                        Some(NetworkAvailability::ReuseOnly)
                    );
                    assert_eq!(chosen(swaps), (None, None), "nothing selects the account");
                    let form = swaps.form.as_ref().unwrap();
                    assert!(
                        matches!(
                            &form.delivery,
                            Err(DeliveryProblem::Network { syncing: false, .. })
                        ),
                        "a new account there needs setup funds: {:?}",
                        form.delivery
                    );
                    assert!(form.quote_delivery().is_none(), "Review stays unavailable");
                    // The picker keeps Polygon and its routes.
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(picker_polygon(swaps, cx), NetworkAvailability::ReuseOnly);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.picker.network, BuyNetworkKey::Chain(137));
                    let content = swaps.buy_picker_content(form, cx);
                    assert!(matches!(
                        &content.tokens,
                        buy_picker::BuyPickerTokens::Listed(rows)
                            if rows.iter().any(|row| row.item.asset.token == STUB_POLYGON_USDT)
                    ));
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                });
            });
            // A funded destination still needs no setup when its existing account is used.
            // Fail the estimator's RPC after sync, without changing its notes or offers.
            session
                .executor_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            let mut offer = crate::root::tests::fee_row(137, STUB_POLYGON_USDT, "reuse-offer");
            offer.relay_adapt_7702 = root.read_with(cx, |root, _| {
                root.effective_chain_configs
                    .get(137)
                    .unwrap()
                    .accepted_executor_profile()
                    .map(wallet_ops::settings::ExecutorProfile::delegate)
            });
            root.update(cx, |root, _| {
                let ChainUtxoState::Ready { snapshot, .. } =
                    root.chain_states.get_mut(&137).unwrap()
                else {
                    panic!("ready destination session")
                };
                *snapshot = Arc::new(wallet_ops::ListUtxosOutput {
                    chain_id: 137,
                    cache_key: "reuse-funding-test".into(),
                    utxo_count: 1,
                    unspent_count: 1,
                    spent_count: 0,
                    local_pending_spent_count: 0,
                    utxos: vec![crate::root::tests::unshield_utxo_output(
                        STUB_POLYGON_USDT,
                        100_000_000,
                        0,
                        1,
                    )],
                    totals: vec![wallet_ops::TokenTotal {
                        token: STUB_POLYGON_USDT.to_string(),
                        total: "100000000".into(),
                        poi_verified_total: "100000000".into(),
                    }],
                });
            });
            // Funds are held there, and no broadcaster offer on the chain has arrived yet.
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                    Some(NetworkAvailability::CheckingFee)
                );
            });
            // An offer only in another token leaves setup unfunded, unless the fee subscription
            // went live so recently that the held token's offer may still arrive.
            root.update(cx, |root, _| {
                root.monitor_state
                    .write()
                    .upsert_fee(crate::root::tests::fee_row(
                        137,
                        STUB_POLYGON_USDC,
                        "other-token-offer",
                    ));
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                    Some(NetworkAvailability::ReuseOnly)
                );
            });
            root.update(cx, |root, _| {
                root.monitor_state.write().start_fee_subscription();
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                    Some(NetworkAvailability::CheckingFee)
                );
            });
            root.update(cx, |root, _| {
                root.monitor_state.write().upsert_fee(offer);
            });
            stubs.set_failing(&["/rpc"]);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(
                        swaps.network_setup_availability(137, cx),
                        NetworkAvailability::CheckingFee
                    );
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(picker_polygon(swaps, cx), NetworkAvailability::CheckingFee);
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.show_buy_picker_network(137, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().picker.network,
                        BuyNetworkKey::Chain(137)
                    );
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                    assert!(swaps.form.as_ref().unwrap().quote_delivery().is_none());
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .picker
                        .funding_checked_for_tests(137)
                })
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_setup_availability(137, cx),
                    NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee),
                    "the actual estimator reports the RPC failure"
                );
            });
            // Polygon catching up with newer blocks keeps the checked estimate, so the quote
            // isn't dropped while the setup fee is checked again.
            let ready = root.update(cx, |root, _| root.chain_states.remove(&137).unwrap());
            let ChainUtxoState::Ready {
                snapshot,
                session: ready_session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ppoi_workflow_status,
            } = &ready
            else {
                panic!("ready destination session")
            };
            root.update(cx, |root, _| {
                root.chain_states.insert(
                    137,
                    ChainUtxoState::Syncing {
                        snapshot: Arc::clone(snapshot),
                        progress: crate::root::chain_load::ChainSyncProgress::default(),
                        catching_up: true,
                        session: Arc::clone(ready_session),
                        observer_token: observer_token.clone(),
                        sync_tip: *sync_tip,
                        poi_refreshing: *poi_refreshing,
                        ppoi_workflow_status: *ppoi_workflow_status,
                    },
                );
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_setup_availability(137, cx),
                    NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee)
                );
            });
            root.update(cx, |root, _| root.chain_states.insert(137, ready));
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(chosen(swaps), (None, None));
                    assert!(swaps.form.as_ref().unwrap().quote_delivery().is_none());
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(
                        picker_polygon(swaps, cx),
                        NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee)
                    );
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.show_buy_picker_network(137, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().picker.network,
                        BuyNetworkKey::Chain(137)
                    );
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                    let select = swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .destination_select
                        .clone()
                        .unwrap();
                    confirm_account(&select, Some(destination), window, cx);
                });
            });
            // The Select event applies after the view's update releases its borrow.
            swaps.read_with(cx, |swaps, _| {
                assert_eq!(chosen(swaps), (None, Some(destination)));
                assert!(swaps.form.as_ref().unwrap().quote_delivery().is_some());
            });
            // Later quote and signing checks still need a working RPC.
            stubs.set_failing(&[]);
            // The existing account is the delivery's receiver and takes no setup: no fee is
            // estimated on Polygon and no broadcaster is chosen there.
            let review = ready_review(swaps, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    let form = swaps.form.as_mut().unwrap();
                    assert!(matches!(
                        &form.delivery,
                        Ok(SwapDelivery::Bridge(delivery))
                            if delivery.receiver == Address::repeat_byte(3)
                    ));
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
                    assert_eq!(form.review_problem(&review), None);
                    assert!(!form.destination_route.is_used());
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(swaps.setup_chain(form, SetupSide::Origin), Some(1));
                    assert_eq!(swaps.setup_chain(form, SetupSide::Destination), None);

                    // The swap's own account is chosen apart from the destination.
                    swaps.select_form_account(Some(operation), window, cx);
                    assert_eq!(chosen(swaps), (Some(operation), Some(destination)));
                    let form = swaps.form.as_ref().unwrap();
                    assert!(!swaps.reviews_setup(form), "neither account needs setup");
                    swaps.select_form_account(None, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("swap-account-source").is_some());
            assert!(cx.debug_bounds("swap-account-destination").is_some());
            assert!(
                cx.debug_bounds("swap-account-select").is_none(),
                "a private Bridge swap has two selectors"
            );
            assert!(cx.debug_bounds("swap-destination-reuse").is_some());
            assert!(cx.debug_bounds("swap-source-reuse").is_none());

            // Another token on Polygon keeps the choice, clears the acknowledgements and
            // checks the delivery to the account again.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_buy_picker(window, cx);
                    swaps.pick_buy_token(STUB_POLYGON_USDC, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    let form = swaps.form.as_ref().unwrap();
                    assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                    swaps.open_buy_picker(window, cx);
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    assert!(matches!(
                        &swaps.form.as_ref().unwrap().delivery,
                        Ok(SwapDelivery::Bridge(delivery))
                            if delivery.receiver == Address::repeat_byte(3)
                    ));
                });
            });
            // Once its records refuse the account, the choice stays and says why.
            polygon.retire(destination).unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)), "the choice stays");
                    let form = swaps.form.as_ref().unwrap();
                    assert!(
                        matches!(&form.delivery, Err(DeliveryProblem::Account(_))),
                        "{:?}",
                        form.delivery
                    );
                    assert!(form.quote_delivery().is_none(), "Review stays unavailable");
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-destination-account-problem")
                    .is_some()
            );

            // Another network returns the destination to a new account. The source stays.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.select_form_account(Some(operation), window, cx);
                    swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-account-select").is_some()
                    && cx.debug_bounds("swap-account-source").is_none()
                    && cx.debug_bounds("swap-account-destination").is_none(),
                "a Public address has one selector"
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    assert_eq!(chosen(swaps), (Some(operation), Some(destination)));
                    assert!(
                        !swaps.form.as_ref().unwrap().blocked_shield_acknowledged,
                        "coming back to a cross-chain Shield asks again"
                    );
                    swaps.open_buy_picker(window, cx);
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.pick_buy_token(STUB_USDT, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.network, None);
                    assert_eq!(form.destination_account, None);
                    assert_eq!(chosen(swaps), (Some(operation), None));
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-account-select").is_some()
                    && cx.debug_bounds("swap-account-source").is_none()
                    && cx.debug_bounds("swap-account-destination").is_none(),
                "a same-chain swap has one selector"
            );

            // Another session's form starts with a new account on each side.
            cx.update(|window, cx| {
                window.close_all_dialogs(cx);
                let fresh = root.update(cx, |root, cx| {
                    root.clear_private_swaps(cx);
                    root.ensure_private_swaps(window, cx);
                    root.private_swaps_view().unwrap()
                });
                assert!(swaps.read(cx).form.is_none());
                fresh.update(cx, |fresh, cx| {
                    fresh.open_new_form(STUB_USDC, window, cx);
                    let form = fresh.form.as_ref().unwrap();
                    assert!(form.operation.is_none() && form.destination_account.is_none());
                    assert!(form.reuse_use.is_none());
                });
            });
            runtime.block_on(session.stop()).unwrap();
        },
    );
}

/// The two setups of a private Bridge swap report their stages by themselves, and one's end
/// leaves the other running.
#[gpui::test]
fn setup_progress_of_each_side_advances_and_ends_by_itself(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus::Pending;

    with_swap_view(cx, |_, swaps, _, operation, _, cx| {
        let ((origin, _origin_route), (destination, _destination_route)) =
            swaps.update(cx, |swaps, cx| {
                (
                    swaps.start_setup_progress(
                        operation,
                        SetupSide::Origin,
                        setup_broadcaster(),
                        cx,
                    ),
                    swaps.start_setup_progress(
                        operation,
                        SetupSide::Destination,
                        setup_broadcaster(),
                        cx,
                    ),
                )
            });
        let running = |side, cx: &mut gpui::VisualTestContext| {
            swaps.read_with(cx, |swaps, _| {
                let progress = swaps.tracking[&operation].setup_progress(side).unwrap();
                progress
                    .steps()
                    .into_iter()
                    .find(|step| step.status == Pending)
                    .map(|step| step.stage)
            })
        };
        origin
            .send(TransactionGenerationStage::ProvingTransaction)
            .unwrap();
        destination
            .send(TransactionGenerationStage::PublishingToBroadcaster)
            .unwrap();
        assert_eq!(
            running(SetupSide::Origin, cx),
            Some(TransactionGenerationStage::ProvingTransaction)
        );
        assert_eq!(
            running(SetupSide::Destination, cx),
            Some(TransactionGenerationStage::PublishingToBroadcaster)
        );

        swaps.update(cx, |swaps, _| {
            swaps.finish_setup_progress(
                operation,
                SetupSide::Origin,
                Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
            );
        });
        assert_eq!(running(SetupSide::Origin, cx), None);
        assert_eq!(
            running(SetupSide::Destination, cx),
            Some(TransactionGenerationStage::PublishingToBroadcaster)
        );
    });
}

/// Stopping a private Bridge swap's setup wait ends the setup that still waited unanswered.
/// The other one's submission already ended, so its broadcaster answered: the job's stop
/// loses that answer, and the setup keeps no stages that would claim none came.
#[gpui::test]
fn stopping_a_setup_wait_leaves_an_answered_setup_to_its_record(cx: &mut TestAppContext) {
    use crate::root::private_swap::setup_progress::{SetupProgress, SetupProgressEnd};

    with_swap_view(cx, |_, swaps, _, operation, runtime, cx| {
        let [(_origin, origin_route), _destination] = swaps.update(cx, |swaps, cx| {
            swaps.job = Some(SwapJob {
                operation,
                kind: SwapJobKind::Setup,
                abort: runtime.spawn(std::future::pending::<()>()).abort_handle(),
            });
            [SetupSide::Origin, SetupSide::Destination].map(|side| {
                let (stages, route) =
                    swaps.start_setup_progress(operation, side, setup_broadcaster(), cx);
                stages
                    .send(TransactionGenerationStage::WaitingForBroadcasterResponse)
                    .unwrap();
                (stages, route)
            })
        });
        // The origin's broadcaster answered: its submission closed its route, and its job
        // still waits for the destination's.
        origin_route.send_modify(|route| route.state = wallet_ops::SubmissionRouteState::Closed);

        swaps.update(cx, |swaps, _| swaps.stop_setup_wait(operation));
        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.job.is_none());
            let tracking = &swaps.tracking[&operation];
            assert!(tracking.setup_progress(SetupSide::Origin).is_none());
            assert_eq!(
                tracking
                    .setup_progress(SetupSide::Destination)
                    .and_then(SetupProgress::end),
                Some(SetupProgressEnd::TimedOut)
            );
        });
    });
}

/// Each account of a private Bridge swap needs setup exactly while it is a new one. The
/// review's setup fees, the fee limits and the accounts its approval binds follow the four
/// combinations, and an existing account's stale estimate is never approved. A broadcaster
/// offer that appears or expires on a reused side leaves the quote and its acknowledgements.
/// Reuse keeps the delivery and shield costs, and the high-cost acknowledgement.
#[gpui::test]
fn private_bridge_setups_follow_each_account(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    // The destination costs of `high_bridge_costs_require_swap_anyway`.
    stubs.set_across_fee_bps(300);
    stubs
        .across_relayer_gas_bps
        .store(200, std::sync::atomic::Ordering::Relaxed);
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            // A setup estimate on each network, as the form's routes would hold them.
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
            let origin_offer =
                setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
            // The estimate's fee reads go over the client it is given.
            let client = runtime
                .block_on(tagged_operation_http().operation_http_client())
                .unwrap();
            let before_estimate = stubs.request_count();
            let origin_estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    None,
                    origin_offer.clone(),
                    &client,
                )))
                .unwrap();
            let routes = stubs.routes_since(before_estimate, "/rpc");
            assert_eq!(
                routes.into_iter().flatten().collect::<Vec<_>>(),
                ["route-0"],
                "the fee reads use only the estimate's own client"
            );
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let polygon_owner = polygon.executor_owner().unwrap();
            polygon_owner
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            let destination_offer = setup_offer(root, 137, STUB_POLYGON_USDT, U256::from(10), cx);
            let destination_estimate = runtime
                .block_on(Box::pin(polygon_owner.estimate_swap_setup_fee(
                    &polygon,
                    None,
                    destination_offer.clone(),
                    &operation_client(root, runtime, cx),
                )))
                .unwrap();
            let (polygon_store, destination) = reusable_polygon_account(root, cx);
            let account = Address::repeat_byte(3).to_checksum(None);
            // Each reused account as its review names it: its number, then its address.
            let numbered = |executors: &ExecutorStore, operation| {
                let records = executors.records().unwrap();
                let index = records
                    .iter()
                    .find(|record| record.operation() == operation)
                    .unwrap()
                    .index();
                format!("#{index}")
            };
            let (own, reused) = (
                numbered(executors, operation),
                numbered(&polygon_store, destination),
            );
            let row = |summary: &SpendAuthorizationSummary, label: &str| {
                summary
                    .rows_for_test()
                    .into_iter()
                    .find(|(row, _)| row == label)
                    .map(|(_, value)| value)
            };
            let reuse_warnings = |summary: &SpendAuthorizationSummary| {
                summary
                    .warnings_for_test()
                    .into_iter()
                    .filter(|warning| warning.starts_with("Reusing"))
                    .collect::<Vec<_>>()
            };

            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    swaps.select_form_destination(Some(destination), window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            // The existing destination still costs its delivery and its shield fee.
            assert!(!private_delivery_cost(&review).is_zero());
            assert!(authorized_high_cost(&review).is_some());
            // The review names both accounts: a new one on Ethereum, and the reused one on
            // Polygon by its number and address, with its own warning and disclosure.
            swaps.read_with(cx, |swaps, cx| {
                let summary = swaps.swap_summary(&review, None, None, None, None, cx);
                assert_eq!(
                    row(&summary, "Source · Ethereum").as_deref(),
                    Some("New account")
                );
                assert_eq!(
                    row(&summary, "Destination · Polygon"),
                    Some(format!("{reused} · {account}"))
                );
                let warnings = reuse_warnings(&summary);
                assert_eq!(warnings.len(), 1, "{warnings:?}");
                assert!(
                    warnings[0].starts_with(&format!("Reusing {reused} on Polygon links")),
                    "{warnings:?}"
                );
                let (_, public, _) = summary.disclosure_for_test().unwrap();
                assert!(
                    public.contains(&format!(
                        "This swap reuses stealth account {reused} on Polygon"
                    )) && !public.contains("on Ethereum, so anyone"),
                    "{public}"
                );
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    assert_eq!(
                        form.review_problem(&review)
                            .map(|problem| problem.to_string()),
                        Some("Confirm Swap anyway to accept the high swap costs.".to_owned())
                    );
                    form.high_costs_acknowledged = true;
                    // A cross-chain Shield has its own acceptance, which later quotes keep.
                    assert_eq!(
                        form.review_problem(&review),
                        Some(BLOCKED_SHIELD_PROBLEM.into())
                    );
                    form.blocked_shield_acknowledged = true;

                    // Offers on Polygon come and go while its account is an existing one.
                    let mut offer = crate::root::tests::fee_row(137, STUB_POLYGON_USDT, "churn");
                    offer.relay_adapt_7702 = destination_offer.relay_adapt_7702;
                    for expiration in [
                        std::time::SystemTime::now() + Duration::from_secs(60),
                        std::time::UNIX_EPOCH,
                    ] {
                        offer.fee_expiration = expiration;
                        let _ = swaps.root.update(cx, |root, _| {
                            root.monitor_state.write().upsert_fee(offer.clone());
                        });
                        swaps.update_setup_route(cx);
                        swaps.refresh_destination_network(window, cx);
                        let form = swaps.form.as_ref().unwrap();
                        assert!(
                            matches!(&form.quote, QuoteState::Ready(current)
                                if Arc::ptr_eq(current, &review)),
                            "an offer on a reused side keeps the quote"
                        );
                        assert!(form.price_acknowledged && form.high_costs_acknowledged);
                        assert_eq!(form.review_problem(&review), None);
                        assert!(!form.destination_route.is_used());
                    }

                    for (source_existing, destination_existing) in
                        [(false, false), (true, false), (false, true), (true, true)]
                    {
                        swaps.select_form_account(source_existing.then_some(operation), window, cx);
                        swaps.select_form_destination(
                            destination_existing.then_some(destination),
                            window,
                            cx,
                        );
                        // Both routes hold an estimate, also one a side no longer needs.
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        form.route.estimate = Some(origin_estimate.clone());
                        form.route.candidates = vec![origin_offer.clone()];
                        form.destination_route.estimate = Some(destination_estimate.clone());
                        form.destination_route.candidates = vec![destination_offer.clone()];
                        let form = swaps.form.as_ref().unwrap();
                        let combination = (source_existing, destination_existing);
                        // The broadcaster controls are those of the accounts that need setup.
                        assert_eq!(
                            (
                                swaps.setup_chain(form, SetupSide::Origin),
                                swaps.setup_chain(form, SetupSide::Destination),
                            ),
                            (
                                (!source_existing).then_some(1),
                                (!destination_existing).then_some(137),
                            ),
                            "{combination:?}"
                        );
                        if source_existing && destination_existing {
                            assert!(!swaps.reviews_setup(form), "no setup is reviewed");
                            continue;
                        }
                        let parts = swaps.setup_parts(form, &review, false).unwrap();
                        let fees = setup_fees(
                            1,
                            parts.origin.as_ref(),
                            parts.destination.as_ref().and_then(DestinationPlan::setup),
                        );
                        assert_eq!(
                            fees.iter().map(|fee| fee.chain_id).collect::<Vec<_>>(),
                            [
                                (!source_existing).then_some(1),
                                (!destination_existing).then_some(137),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>(),
                            "{combination:?}"
                        );
                        let limit = |estimate: &ExecutorRecoveryFeeEstimate| SwapSetupFeeLimit {
                            token: estimate.broadcaster().token,
                            amount: default_public_broadcaster_fee_limit(estimate.fee_amount()),
                        };
                        let bounds = &parts.approval.bounds;
                        assert_eq!(
                            (bounds.source_setup_fee, bounds.destination_setup_fee),
                            (
                                (!source_existing).then(|| limit(&origin_estimate)),
                                (!destination_existing).then(|| limit(&destination_estimate)),
                            ),
                            "{combination:?}"
                        );
                        let accounts = parts.approval.accounts.unwrap();
                        let account = Address::repeat_byte(3);
                        assert_eq!(
                            accounts.source,
                            SwapApprovedAccount {
                                address: source_existing.then_some(account),
                                setup: !source_existing,
                            },
                            "{combination:?}"
                        );
                        assert_eq!(
                            accounts.destination,
                            Some(SwapApprovedAccount {
                                address: destination_existing.then_some(account),
                                setup: !destination_existing,
                            }),
                            "{combination:?}"
                        );
                        assert_eq!(
                            parts.destination.as_ref().and_then(|plan| match plan {
                                DestinationPlan::Existing(account) => Some(account.operation),
                                DestinationPlan::Setup(_) => None,
                            }),
                            destination_existing.then_some(destination),
                        );
                    }
                });
            });

            // With both accounts existing ones, the swap is quoted for the chosen account on
            // the stub route again. Its review names each account and warns for each, and
            // Review opens the order's review at once: no setup is reviewed or waited for.
            let orderbook = stub_orderbook(&stubs, runtime);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                    form.orderbook = Some(orderbook);
                    swaps.schedule_quote(window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let summary = swaps.swap_summary(&review, None, None, None, None, cx);
                    assert_eq!(
                        row(&summary, "Source · Ethereum"),
                        Some(format!("{own} · {account}"))
                    );
                    assert_eq!(
                        row(&summary, "Destination · Polygon"),
                        Some(format!("{reused} · {account}"))
                    );
                    let warnings = reuse_warnings(&summary);
                    assert!(
                        warnings.len() == 2
                            && warnings[0].starts_with(&format!("Reusing {own} on Ethereum"))
                            && warnings[1].starts_with(&format!("Reusing {reused} on Polygon")),
                        "{warnings:?}"
                    );
                    let (_, public, _) = summary.disclosure_for_test().unwrap();
                    for reuse in [
                        format!("This swap reuses stealth account {own} on Ethereum"),
                        format!("This swap reuses stealth account {reused} on Polygon"),
                    ] {
                        assert!(public.contains(&reuse), "{public}");
                    }
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    let draft = form.reuse_use;
                    swaps.form_primary(window, cx);
                    let pending = swaps
                        .pending_authorization
                        .as_ref()
                        .unwrap_or_else(|| panic!("{:?}", swaps.form.as_ref().unwrap().error));
                    let SwapAction::Order(approval) = &pending.action else {
                        panic!("both existing accounts go straight to the order's review");
                    };
                    assert!(approval.full_review);
                    assert_eq!(approval.swap_use, draft);
                    assert_eq!(
                        approval.pair_destination.map(|account| account.operation),
                        Some(destination)
                    );
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// The destination select works from the keyboard like any other select: the arrow keys open
/// it and move through its accounts, Enter chooses one, and focus is back on the select.
#[gpui::test]
fn private_bridge_destination_is_chosen_from_the_keyboard(cx: &mut TestAppContext) {
    use gpui::Focusable as _;
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let (_, destination) = reusable_polygon_account(root, cx);
            let select = cx.update(|window, cx| {
                let select = swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.destination_choice(), None, "a new account by default");
                    form.destination_select.clone().unwrap()
                });
                window.draw(cx).clear(cx);
                // As Tab does from the source select.
                select.update(cx, |select, cx| select.focus(window, cx));
                window.draw(cx).clear(cx);
                select
            });
            let focused = |cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    select.read(cx).focus_handle(cx).is_focused(window)
                })
            };
            assert!(focused(cx));

            // Down opens the list on the selected new account, and again moves to the
            // existing one below it.
            cx.simulate_keystrokes("down");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("down enter");
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.destination_choice().map(|account| account.operation),
                    Some(destination)
                );
                assert_eq!(form.operation, None, "the source stays a new account");
            });
            assert!(focused(cx), "focus returns to the select it was chosen in");
            runtime.block_on(session.stop()).unwrap();
        },
    );
}
