use super::*;

#[gpui::test]
fn public_buy_picker_keeps_same_asset_routes_and_refuses_own_chain_and_unfunded_destination(
    cx: &mut TestAppContext,
) {
    use alloy::primitives::address;
    use wallet_ops::bridge::{
        AcrossRoute, PublicBridgePath, PublicSellAsset, public_across_destination_tokens,
    };
    let sell = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let received = address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            let account = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    enable_stub_chain(root, &stubs, 137);
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(account, sell, window, cx);
                    let root = swaps.root.upgrade().unwrap();
                    let routes = public_across_destination_tokens(
                        &[AcrossRoute {
                            origin_token: sell,
                            destination_token: received,
                            origin_symbol: "USDC".into(),
                            destination_symbol: "USDC".into(),
                        }],
                        PublicSellAsset::Erc20(sell),
                        false,
                        &root.read(cx).effective_token_registry,
                        1,
                        137,
                    );
                    assert_eq!(routes[0].path, PublicBridgePath::Deposit);
                    let form = swaps.form.as_mut().unwrap();
                    form.public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((sell, 137), routes);
                    form.picker.network = BuyNetworkKey::Chain(137);
                    form.picker.open = true;
                    let form = swaps.form.as_ref().unwrap();
                    let items = swaps.buy_picker_items(form, cx);
                    assert!(
                        items.iter().any(|item| item.asset.token == received),
                        "a direct route remains selectable when there is no CoW route"
                    );
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 1, cx),
                        Some(NetworkAvailability::Unavailable(NetworkUnavailable::Shield))
                    );
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                        Some(NetworkAvailability::Unavailable(
                            NetworkUnavailable::Unfunded
                        ))
                    );
                    swaps.show_buy_picker_network(1, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().picker.network,
                        BuyNetworkKey::Chain(137),
                        "the own-chain Shield pointer cannot become a bridge destination"
                    );
                    let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                    assert!(
                        content.receive_to == ReceiveTo::PrivateBalance
                            && content.receive_to_locked,
                        "the switch shows Private selected and can't change"
                    );
                    swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().receive_to,
                        ReceiveTo::PrivateBalance
                    );
                    swaps.form.as_mut().unwrap().picker.open = false;
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

// A chain with a Public swap profile takes no orders until its math contract is deployed. Its
// form then lists what a deposit serves and says so without blocking Review, as on a chain
// that has no profile. A form opened once the contract is there lists the other tokens too.
#[gpui::test]
fn public_form_lists_only_direct_routes_until_its_chain_takes_orders(cx: &mut TestAppContext) {
    use wallet_ops::bridge::PublicBridgePath;
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
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let key = (STUB_USDT, 137);
            for available in [false, true] {
                stubs.set_math_deployed(available);
                // Each form reads its chain again.
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.open_public_form(source.clone(), STUB_USDT, window, cx);
                        swaps.form.as_mut().unwrap().network = Some(137);
                        swaps.load_bridge_routes(window, cx);
                    });
                });
                drive_until(cx, runtime, |cx| {
                    swaps.read_with(cx, |swaps, _| {
                        let public = swaps.form.as_ref().unwrap().public.as_ref().unwrap();
                        public.orders_available.is_some() && public.routes.contains_key(&key)
                    })
                });
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        let form = swaps.form.as_mut().unwrap();
                        let public = form.public.as_ref().unwrap();
                        assert_eq!(public.orders_available, Some(available));
                        let listed = public.routes[&key]
                            .iter()
                            .map(|route| (route.destination.destination_token, route.path))
                            .collect::<Vec<_>>();
                        let direct = (STUB_POLYGON_USDT, PublicBridgePath::Deposit);
                        if available {
                            // Every other token Across delivers there is listed as an order.
                            assert!(listed.contains(&direct));
                            assert!(listed.contains(&(STUB_POLYGON_USDC, PublicBridgePath::Order)));
                            assert!(listed.iter().all(|route| *route == direct
                                || route.1 == PublicBridgePath::Order));
                        } else {
                            assert_eq!(listed, [direct]);
                        }
                        // The token that bridges directly can be picked either way.
                        form.buy = Some(STUB_POLYGON_USDT);
                        let reason = swaps.public_form_reason(swaps.form.as_ref().unwrap(), cx);
                        if available {
                            assert!(reason.is_none());
                        } else {
                            let reason = reason.expect("a chain without orders says so");
                            assert!(!reason.blocks_review);
                            assert_eq!(
                                reason.text,
                                format!(
                                    "From {}, a token can only be bridged as itself, such as USDC to USDC. Swapping to a different token isn't supported on this network.",
                                    network_name(1)
                                )
                            );
                        }
                        swaps.close_swap_dialog(window, cx);
                    });
                });
                cx.run_until_parked();
            }
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn held_public_destination_shows_the_held_amount_and_the_recovery_action(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::Bytes;
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        Call, CommitmentPreimage, RelayAdapt7702, ShieldCiphertext, ShieldRequest, TokenData,
        shieldCall,
    };
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, PublicSwapObservations, SwapBridgeHandoff,
        SwapBridgeOutcome, SwapObservation,
    };
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        let observed = SwapObservation {
            block: BlockNumHash::new(20, B256::ZERO),
            transaction_hash: Some(B256::ZERO),
        };
        let held = PublicSwapObservations {
            bridge_handoff: Some(SwapBridgeHandoff {
                observation: observed,
                deposit_id: Some(U256::ONE),
            }),
            bridge_outcome: Some(SwapBridgeOutcome::HeldOnDestination {
                block: observed.block,
                transaction_hash: B256::ZERO,
                amount: U256::from(100),
            }),
            ..Default::default()
        };
        let record = model::tests::public_presentation_record(true, &held);
        let identity = model::SwapIdentity {
            operation: record.operation(),
            swap_use: record.swap_uses()[0].id(),
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.public_records = vec![(1, record.clone())];
                swaps.show_view(SwapDialogView::PublicDetail(identity), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let (record, claimed, swap) = swaps.public_swap_record(identity).unwrap();
            let stage = model::public_swap_stage(
                record,
                claimed,
                None,
                now_unix(),
                swaps.attribution(record),
                swaps.public_destination_balance(record, claimed),
            )
            .unwrap();
            assert_eq!(stage, model::PublicSwapStage::HeldOnDestination);
            // The wallet has no metadata for the held token: the detail keeps the held
            // amount as a private swap words it, and the card's title names only the token.
            let held = swaps.public_labels(record, claimed, cx).unwrap().held;
            assert!(held.as_deref().is_some_and(|held| held.starts_with("100 ")));
            let title = swaps.public_card_line(cx).unwrap().title;
            let held_on = format!(" held on {}", network_name(1));
            assert!(
                !title.starts_with("100") && title.ends_with(&held_on),
                "{title}"
            );
            assert!(
                swaps
                    .shown_public_swaps()
                    .any(|(shown, _)| shown == identity)
            );
            assert!(
                public_progress::public_swap_actions(swap, claimed, stage, now_unix())
                    .contains(&public_progress::PublicSwapAction::RecoverDestination)
            );
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, _| {
            use gpui_kit::test::TestWindowExt;
            assert!(
                window
                    .find("public-swap-action-RecoverDestination")
                    .visible()
            );
        });

        // A completed recovery returns only its requested amount. A different asset's
        // recovery says nothing about these proceeds, even if an explicit check reads zero.
        let recovered = |token: Address| {
            let request = ShieldRequest {
                preimage: CommitmentPreimage {
                    npk: B256::repeat_byte(0x71),
                    token: TokenData::erc20(token),
                    value: alloy::primitives::Uint::from(10_u64),
                },
                ciphertext: ShieldCiphertext {
                    encryptedBundle: [B256::ZERO; 3],
                    shieldKey: B256::ZERO,
                },
            };
            let before =
                ExecutorNonceObservation::new(BlockNumHash::new(29, B256::ZERO), U256::ZERO);
            let calldata = RelayAdapt7702::multicallCall {
                _requireSuccess: true,
                _calls: vec![Call {
                    to: record.address().unwrap(),
                    data: shieldCall {
                        _shieldRequests: vec![request.clone()],
                    }
                    .abi_encode()
                    .into(),
                    value: U256::ZERO,
                }],
                _nonce: U256::ZERO,
                _signature: Bytes::new(),
            }
            .abi_encode();
            let payload = IssuedExecutorPayload::new(
                U256::ZERO,
                record.delegate(),
                B256::repeat_byte(0x71),
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(calldata.into(), before, Vec::new()),
            );
            let mut saved = serde_json::to_value(&record).unwrap();
            saved["issued"] = serde_json::json!([payload]);
            saved["nonce_watermark"] =
                serde_json::to_value(ExecutorNonceWatermark::new(U256::ONE, 31)).unwrap();
            (
                serde_json::from_value::<ExecutorRecord>(saved).unwrap(),
                wallet_ops::ExecutorAttribution::with_shields_for_tests(
                    Address::ZERO,
                    &[(request, 30)],
                ),
            )
        };
        let (partial, relevant) = recovered(Address::repeat_byte(7));
        let (unrelated, other_asset) = recovered(Address::repeat_byte(8));
        let at = |amount, number| Some((U256::from(amount), BlockNumHash::new(number, B256::ZERO)));
        let cases = [
            // No explicit read, including after a failed refresh, keeps recovery available.
            (partial.clone(), relevant.clone(), None, 100, false),
            (partial.clone(), relevant.clone(), at(90_u64, 31), 90, false),
            // A read before the fill isn't a current held amount or completion evidence.
            (partial.clone(), relevant.clone(), at(0_u64, 19), 100, false),
            // Zero before the recovery's shield can't establish that recovery emptied it.
            (partial.clone(), relevant.clone(), at(0_u64, 29), 0, false),
            (unrelated, other_asset, at(0_u64, 31), 0, false),
            (partial, relevant, at(0_u64, 31), 0, true),
        ];
        for (record, attribution, balance, amount, complete) in cases {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.public_records = vec![(1, record)];
                    swaps.attributions.insert(identity.operation, attribution);
                    swaps.public_destination_balances.clear();
                    if let Some(balance) = balance {
                        swaps
                            .public_destination_balances
                            .insert((1, identity.operation, identity.swap_use), balance);
                    }
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            swaps.read_with(cx, |swaps, cx| {
                let (record, claimed, swap) = swaps.public_swap_record(identity).unwrap();
                let stage = model::public_swap_stage(
                    record,
                    claimed,
                    None,
                    now_unix(),
                    swaps.attribution(record),
                    swaps.public_destination_balance(record, claimed),
                )
                .unwrap();
                assert_eq!(
                    stage,
                    if complete {
                        model::PublicSwapStage::Recovered
                    } else {
                        model::PublicSwapStage::HeldOnDestination
                    },
                );
                let label = swaps
                    .public_labels(record, claimed, cx)
                    .unwrap()
                    .held
                    .unwrap();
                assert!(label.starts_with(&format!("{amount} ")), "{label}");
                let actions =
                    public_progress::public_swap_actions(swap, claimed, stage, now_unix());
                assert_eq!(
                    actions.contains(&public_progress::PublicSwapAction::RecoverDestination),
                    !complete,
                );
                assert_eq!(
                    actions.contains(&public_progress::PublicSwapAction::CheckStatus),
                    !complete,
                );
                assert_eq!(
                    swaps
                        .shown_public_swaps()
                        .any(|(shown, _)| shown == identity),
                    !complete,
                );
                // The confirmed read never rewrites the bridge's original fill evidence.
                assert!(matches!(
                    swap.observations().bridge_outcome,
                    Some(SwapBridgeOutcome::HeldOnDestination { amount, block, .. })
                        if amount == U256::from(100) && block.number == 20
                ));
            });
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.render_frame(cx);
                assert_eq!(
                    window
                        .try_find("public-swap-action-RecoverDestination")
                        .is_some_and(|button| button.visible()),
                    !complete,
                );
            });
        }
    });
}

#[gpui::test]
fn retrying_an_expired_public_order_starts_a_fresh_review_and_preserves_its_history(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        AcrossOrderTerms, PublicSwapClaim, PublicSwapObservations, SwapAccountChoice,
        SwapObservation,
    };
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
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
            let account = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let template =
                model::tests::public_presentation_record(true, &PublicSwapObservations::default());
            let saved = template.swap_uses()[0].public_swap().unwrap();
            let mut approval = saved.approval().clone();
            approval.sell_token = Address::repeat_byte(42);
            approval.bounds.sell_amount = U256::from(7_000_000_000_000_000_000_u64);
            approval.bounds.slippage_bps = 125;
            approval.bounds.gas_share_bps = Some(3_000);
            approval.destination.address = destination.address();
            let id = SwapUseId::random().unwrap();
            destination_store
                .claim_public_swap(PublicSwapClaim {
                    id,
                    origin_chain: 1,
                    source: account.address,
                    source_scope: account.scope,
                    account: SwapAccountChoice::Existing(operation),
                    delegate: destination.delegate(),
                    destination_token: STUB_POLYGON_USDC,
                    bridged_token: saved.intent().bridged_token,
                    order: true,
                    approval: approval.clone(),
                    now: now_unix(),
                })
                .unwrap();
            destination_store
                .record_public_swap_path(
                    operation,
                    id,
                    saved.path().unwrap().clone(),
                    AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(8),
                        input_token: saved.intent().bridged_token,
                        output_token: STUB_POLYGON_USDC,
                        input_amount: U256::from(995),
                        output_amount: U256::from(990),
                        quote_timestamp: 0,
                        fill_deadline: 100,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: destination.address(),
                        message_hash: Some(B256::ZERO),
                    },
                )
                .unwrap();
            let old = destination_store
                .record_public_swap_observations(
                    operation,
                    id,
                    PublicSwapObservations {
                        expired: Some(SwapObservation {
                            block: BlockNumHash::new(20, B256::ZERO),
                            transaction_hash: None,
                        }),
                        ..Default::default()
                    },
                )
                .unwrap();
            let identity = model::SwapIdentity {
                operation,
                swap_use: id,
            };
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.public_records = vec![(137, old.clone())];
                    swaps.show_view(SwapDialogView::PublicDetail(identity), window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.click("public-swap-action-Retry", cx);
            });
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let public = form.public.as_ref().unwrap();
            assert_eq!(public.account.public_account_uuid, account.public_account_uuid);
            assert!(public.operation.is_none() && public.swap_use.is_none(), "the signed use is immutable and cannot own the new review");
            assert!(public.review.is_none(), "the old signature and quote cannot authorize another attempt");
            assert_eq!((form.sell, form.network, form.buy), (approval.sell_token, Some(137), Some(STUB_POLYGON_USDC)));
            assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
            assert_eq!((form.slippage_bps, form.gas_share_bps), (125, 3_000));
            assert!(form.destination_account.is_none(), "the previously claimed account is checked afresh instead of selected automatically");
            assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
        });
            assert_eq!(
                destination_store
                    .records()
                    .unwrap()
                    .into_iter()
                    .find(|record| record.operation() == operation)
                    .unwrap(),
                old
            );
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// Activity opens a swap from a click outside any root update. Showing a cancelled swap
/// reads the root for its tokens, which a still-leased root would turn into a panic.
#[gpui::test]
fn activity_opens_a_cancelled_swap_without_holding_the_root(cx: &mut TestAppContext) {
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let swap_use = executors
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .and_then(|record| record.active_swap_use())
            .unwrap();
        executors.cancel_swap_use(operation, swap_use).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, _| swaps.reload_records());
            crate::root::wallet_activity::open_activity_swap(root, swap_use, window, cx);
            assert!(window.has_active_dialog(cx));
        });
    });
}
