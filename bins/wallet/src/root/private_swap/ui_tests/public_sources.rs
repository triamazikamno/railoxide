use super::*;

/// A chosen stealth account spends the private balance, so Pay from is off until the form is
/// back on a new account, and a choice the control committed anyway is put back.
#[gpui::test]
fn pay_from_is_fixed_while_an_existing_stealth_account_is_chosen(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, _, _, cx| {
            reusable_account(executors, operation);
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
                swaps.update(cx, |swaps, cx| {
                    swaps.open_new_form(STUB_USDC, window, cx);
                    swaps.select_form_account(Some(operation), window, cx);
                });
            });
            let swap_use = swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_use.is_some());
                form.reuse_use
            });

            assert!(
                !pay_from_menu_opens(cx),
                "the payer can't change while an existing account is chosen"
            );
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert!(form.public.is_none());
                assert_eq!(form.pay_from_select.read(cx).selected_value(), Some(&None));
            });

            // The control commits its choice before it reports it to the form.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let select = swaps.form.as_ref().unwrap().pay_from_select.clone();
                    let public = Some("account-1".to_owned());
                    select.update(cx, |select, cx| {
                        select.set_selected_value(&public, window, cx);
                    });
                    assert_eq!(select.read(cx).selected_value(), Some(&public));
                    swaps.select_form_source(public.as_deref(), window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert!(form.public.is_none());
                    assert_eq!(
                        form.pay_from_select.read(cx).selected_value(),
                        Some(&None),
                        "a rejected payer doesn't stay shown"
                    );
                    assert_eq!(
                        (form.operation, form.reuse_use),
                        (Some(operation), swap_use)
                    );

                    swaps.select_form_account(None, window, cx);
                });
            });
            choose_swap_source(cx, "swap-source-public-account-1");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.public
                        .as_ref()
                        .map(|public| public.account.public_account_uuid.as_str()),
                    Some("account-1"),
                    "a new account lets a Public account pay"
                );
            });
        },
    );
}

#[gpui::test]
fn pay_from_changes_clear_acknowledgements_and_global_imports_cannot_be_chosen(
    cx: &mut TestAppContext,
) {
    let stubs = SwapStubs::start();
    stubs.enable_public_reviews();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            let sell = STUB_USDC;
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    root.public_accounts.truncate(4);
                    root.public_accounts[3].source =
                        wallet_ops::vault::PublicAccountSource::Imported;
                    root.public_accounts[3].scope = wallet_ops::vault::PublicAccountScope::Global;
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
                swaps.update(cx, |swaps, cx| {
                    swaps.open_new_form(sell, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.destination_route.fee_token = Some(sell);
                    form.destination_route.fee_token_chosen = true;
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (destination_store, destination_operation) = reusable_polygon_account(root, cx);
            let destination_record = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == destination_operation)
                .unwrap();
            let (review, across, orderbook, route) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                sell,
                U256::from(7_000_000),
                false,
                cx,
            );
            choose_swap_source(cx, "swap-source-public-account-1");
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.public.as_ref().unwrap().account.public_account_uuid,
                    "account-1"
                );
                assert_eq!(
                    form.sell, sell,
                    "the account holds the previously selected Sell token"
                );
                assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                assert!(
                    form.account_select.is_none(),
                    "a Public source creates no source stealth account"
                );
                assert!(form.destination_select.is_some());
                assert!(
                    !form.destination_route.is_used(),
                    "another payer must review setup terms again"
                );
                assert_eq!(form.assets.locked, U256::ZERO);
                assert!(
                    form.assets
                        .sell_assets
                        .iter()
                        .any(|asset| asset.token == Address::ZERO)
                );
                assert!(swaps.receive_to_locked(form));
            });
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.destination_account = Some(DestinationAccount {
                        chain_id: 137,
                        operation: destination_operation,
                        index: destination_record.index(),
                        address: destination_record.address().unwrap(),
                    });
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((sell, 137), vec![route]);
                    swaps.install_public_review_for_tests(
                        review.clone(),
                        across.clone(),
                        Some(orderbook.clone()),
                        cx,
                    );
                    assert!(
                        swaps
                            .form
                            .as_ref()
                            .unwrap()
                            .public
                            .as_ref()
                            .unwrap()
                            .review
                            .is_some()
                    );
                });
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_form_sell(STUB_USDT, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(
                        (form.network, form.buy),
                        (Some(137), Some(STUB_POLYGON_USDC))
                    );
                    assert!(
                        form.public.as_ref().unwrap().review.is_none(),
                        "an unserved Sell pair cannot retain the previous token's received minimum"
                    );
                    assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                    swaps.request_public_review(window, cx);
                    assert!(
                        swaps.public_authorization.is_none(),
                        "Review waits for the new pair's route"
                    );
                    swaps.set_form_sell(sell, window, cx);
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                });
            });
            choose_swap_source(cx, "swap-source-public-account-2");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                let public = form.public.as_ref().unwrap();
                assert_eq!(public.account.public_account_uuid, "account-2");
                assert!(public.review.is_none() && public.routes.is_empty());
                assert_eq!(
                    (form.network, form.buy),
                    (Some(137), Some(STUB_POLYGON_USDC)),
                    "a payer change preserves the chosen destination without its old quote"
                );
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.request_public_review(window, cx));
            });
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_authorization.is_none(),
                    "the new payer has no route and cannot approve the previous payer's minimum"
                );
            });
            choose_swap_source(cx, "swap-source-public-account-3");
            swaps.read_with(cx, |swaps, _| {
                assert_eq!(
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .public
                        .as_ref()
                        .unwrap()
                        .account
                        .public_account_uuid,
                    "account-2",
                    "a globally visible import cannot admit a Public swap"
                );
            });
            swaps.read_with(cx, |swaps, cx| {
                let groups = swaps.swap_source_items(sell, cx);
                // Private balance, the heading of the Public accounts, then the accounts.
                assert_eq!(groups.len(), 1);
                assert!(groups[0].items[0].account.is_none());
                assert!(groups[0].items[1].heading && groups[0].items[1].unavailable);
                let (shared, scoped): (Vec<_>, Vec<_>) = groups[0].items[2..]
                    .iter()
                    .partition(|item| item.account.as_deref() == Some("account-3"));
                assert!(shared[0].unavailable);
                assert_eq!(
                    shared[0].reason.as_deref(),
                    Some(SHARED_ACCOUNT_REASON),
                    "Pay from says why an account shared between Private wallets can't pay"
                );
                assert!(
                    scoped
                        .iter()
                        .all(|item| !item.unavailable && item.reason.is_none())
                );
                // An account whose balances are read holds none of a token they don't list.
                let unheld = Address::repeat_byte(0x77);
                let zero = format!("0 {}", swaps.token_symbol(unheld, cx));
                let read = root
                    .read(cx)
                    .public_balance_snapshot
                    .as_ref()
                    .unwrap()
                    .accounts
                    .iter()
                    .map(|account| account.account.public_account_uuid.clone())
                    .collect::<Vec<_>>();
                let groups = swaps.swap_source_items(unheld, cx);
                let rows = groups[0]
                    .items
                    .iter()
                    .filter(|item| {
                        item.account
                            .as_ref()
                            .is_some_and(|uuid| read.contains(uuid))
                    })
                    .collect::<Vec<_>>();
                assert!(
                    !rows.is_empty() && rows.iter().all(|item| item.balance == zero),
                    "a read account without the Sell token shows a zero balance"
                );
            });
            choose_swap_source(cx, "swap-source-private");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert!(form.public.is_none());
                assert!(
                    form.account_select.is_some(),
                    "Private balance retains the existing account flow"
                );
                assert_eq!(form.receive_to, ReceiveTo::PrivateBalance);
                assert!(!swaps.receive_to_locked(form));
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn public_bridge_opens_before_a_destination_or_origin_private_session_exists(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, _, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.clear_private_swaps(cx);
                root.chain_states.remove(&1);
                root.open_public_swap_form("account-1", Some(Address::repeat_byte(42)), window, cx);
            });
        });
        cx.run_until_parked();
        let swaps = root.read_with(cx, |root, _| root.private_swaps_view().unwrap());
        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.session.is_none() && swaps.owner.is_none());
            assert_eq!(swaps.origin_chain_id, 1);
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                form.public.as_ref().unwrap().account.public_account_uuid,
                "account-1"
            );
            assert!(
                form.network.is_none(),
                "opening does not bind an arbitrary destination owner"
            );
            assert!(form.account_select.is_none());
            assert!(form.destination_select.is_some());
        });
        choose_swap_source(cx, "swap-source-private");
        swaps.read_with(cx, |swaps, _| {
            assert!(
                swaps.form.as_ref().unwrap().public.is_some(),
                "Private balance cannot be selected without a source private session"
            );
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    });
}

#[gpui::test]
fn public_route_error_retry_discards_the_client_and_preserves_the_setup(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    stubs.enable_public_reviews();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (destination_store, operation) = reusable_polygon_account(root, cx);
            let destination = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let (review, across, orderbook, _) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            let use_id = SwapUseId::random().unwrap();
            let revision = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source, STUB_USDT, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    let form = swaps.form.as_mut().unwrap();
                    let public = form.public.as_mut().unwrap();
                    public.operation = Some(operation);
                    public.swap_use = Some(use_id);
                    public.route_tasks.clear();
                    public.routes.clear();
                    public.route_errors.insert(
                        (STUB_USDT, 137),
                        "Across rejected the routes request (HTTP 403).".into(),
                    );
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.quote_task =
                        Some(cx.spawn(async move |_, _| std::future::pending::<()>().await));
                    cx.notify();
                    form.quote_revision
                })
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let retry = cx
                .debug_bounds("swap-bridge-routes-retry")
                .expect("the route error offers Retry");
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                let public = form.public.as_ref().unwrap();
                assert!(
                    public.quote_route_is_invalidated_for_tests(),
                    "Retry discards the isolated operation client and old review"
                );
                assert!(
                    form.quote_task.is_none() && form.quote_revision != revision,
                    "an old preview cannot restore the discarded client"
                );
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                assert_eq!(
                    (public.operation, public.swap_use),
                    (Some(operation), Some(use_id))
                );
                assert_eq!(
                    (form.sell, form.network, form.buy),
                    (STUB_USDT, Some(137), Some(STUB_POLYGON_USDC))
                );
                assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                assert!(public.route_errors.is_empty());
                assert!(
                    public.route_tasks.contains_key(&(STUB_USDT, 137)),
                    "Retry restarts route discovery"
                );
            });
            assert_eq!(
                destination_store
                    .records()
                    .unwrap()
                    .into_iter()
                    .find(|record| record.operation() == operation)
                    .unwrap(),
                destination
            );
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    swaps
                        .form
                        .as_mut()
                        .unwrap()
                        .public
                        .as_mut()
                        .unwrap()
                        .route_tasks
                        .clear();
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}
