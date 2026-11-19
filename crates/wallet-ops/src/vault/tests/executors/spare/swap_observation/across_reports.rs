use super::*;

/// Anyone can emit a matching fill event, so one counts only from the destination chain's pinned
/// `SpokePool`, or beside the output token's transfer of the signed amount to the receiver in
/// the same receipt, which then counts once for the amount transferred. A final block without
/// such a fill leaves the delivery as Across's report.
#[tokio::test]
async fn across_fills_need_the_pinned_pool_or_the_token_transfer_and_are_otherwise_reported() {
    enum Expected {
        /// By the destination chain's transaction at this index, for this much above the signed
        /// amount.
        Verified(usize, u64),
        Reported,
        Ambiguous,
    }
    /// What a forged event claims to have executed above the signed amount.
    const FORGED: u64 = 1_000_000;
    let (_, terms) = across_order(Address::ZERO);
    let receiver = Address::repeat_byte(0x77);
    let (emitter, relayer) = (Address::repeat_byte(0x5e), Address::repeat_byte(0x55));
    let other_emitter = Address::repeat_byte(0x5f);
    let pinned =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(42161)
            .unwrap()
            .bridge_destination()
            .unwrap()
            .spoke_pool()
            .unwrap();
    let paid = |token: Address, value: U256| {
        (
            token,
            Transfer {
                from: relayer,
                to: receiver,
                value,
            }
            .encode_log_data(),
        )
    };
    let payment = paid(terms.output_token, terms.output_amount);
    // Each receipt of the fill block as its fill events, by emitter and the amount executed
    // above the signed one, and the transfers after them.
    for (scenario, (chain_id, receipts, expected)) in [
        // A chain without a pinned pool, and one whose pinned pool no longer emits the fill.
        (
            UNPINNED_CHAIN,
            vec![(vec![(emitter, 0)], vec![payment.clone()])],
            Expected::Verified(0, 0),
        ),
        (
            42161,
            vec![(vec![(emitter, 0)], vec![payment.clone()])],
            Expected::Verified(0, 0),
        ),
        // No transfer, as for a native delivery: the fill event alone proves nothing.
        (
            UNPINNED_CHAIN,
            vec![(vec![(emitter, 0)], vec![])],
            Expected::Reported,
        ),
        // Less than the signed amount, and the signed amount of another token.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![paid(terms.output_token, terms.output_amount - U256::ONE)],
            )],
            Expected::Reported,
        ),
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![paid(OTHER_BUY, terms.output_amount)],
            )],
            Expected::Reported,
        ),
        // The signed amount of the output token, paid to someone other than the receiver.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![(
                    terms.output_token,
                    Transfer {
                        from: relayer,
                        to: Address::repeat_byte(0x78),
                        value: terms.output_amount,
                    }
                    .encode_log_data(),
                )],
            )],
            Expected::Reported,
        ),
        // Two receipts with evidence.
        (
            UNPINNED_CHAIN,
            vec![
                (vec![(emitter, 0)], vec![payment.clone()]),
                (vec![(other_emitter, 0)], vec![payment.clone()]),
            ],
            Expected::Ambiguous,
        ),
        // A forged event without evidence doesn't make the real fill ambiguous.
        (
            42161,
            vec![(vec![(emitter, 0)], vec![]), (vec![(pinned, 0)], vec![])],
            Expected::Verified(1, 0),
        ),
        // Nor does one in the real fill's receipt: its events are one candidate, for the
        // smallest qualifying transfer and not for an amount an event claims.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0), (other_emitter, FORGED)],
                vec![
                    paid(terms.output_token, terms.output_amount + U256::from(50)),
                    paid(terms.output_token, terms.output_amount + U256::from(3)),
                ],
            )],
            Expected::Verified(0, 3),
        ),
        // The pinned pool's event gives the amount, whatever is logged before it.
        (
            42161,
            vec![(vec![(emitter, FORGED), (pinned, 5)], vec![payment.clone()])],
            Expected::Verified(0, 5),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, chain_id).await;
        let (destination, config, server) = across_destination(chain_id).await;
        {
            let mut chain = destination.lock().unwrap();
            for (events, transfers) in receipts {
                let to = events[0].0;
                let mut logs = events
                    .into_iter()
                    .map(|(emitter, above)| {
                        let mut fill = filled_relay(delivery, &terms, 42);
                        fill.relayExecutionInfo.updatedOutputAmount += U256::from(above);
                        (emitter, fill.encode_log_data())
                    })
                    .collect::<Vec<_>>();
                logs.extend(transfers);
                chain.add_addressed_transaction(30, to, Bytes::new(), logs);
            }
            chain.head = 31;
        }
        let (url, _, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(|_| {
                across_report("filled", REPORTED_AMOUNT)
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let result = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        let (outcome, expected_state) = {
            let chain = destination.lock().unwrap();
            assert_block_only_requests(&chain.rpc_requests, chain.block(30));
            match expected {
                Expected::Verified(index, above) => {
                    let mut verified = across_verified(&chain, &terms, index);
                    if let SwapBridgeOutcome::DeliveredVerified { output_amount, .. } =
                        &mut verified
                    {
                        *output_amount += U256::from(above);
                    }
                    (Some(verified), SwapOrderState::Done)
                }
                Expected::Reported => (Some(across_reported()), SwapOrderState::Done),
                Expected::Ambiguous => (None, SwapOrderState::Bridging),
            }
        };
        match expected {
            Expected::Ambiguous => assert!(result.is_err(), "scenario {scenario}"),
            _ => assert_eq!(result.unwrap(), outcome, "scenario {scenario}"),
        }
        let record = fixture.record();
        assert_eq!(bridge_outcome(&record), outcome, "scenario {scenario}");
        assert_eq!(state(&record, 0), expected_state, "scenario {scenario}");
        // A report is final for routine polling, like a verified delivery.
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(outcome.is_none()),
            "scenario {scenario}"
        );
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

/// A report is final, so only a read that establishes the fill block holds no fill, or that
/// every Sensitive endpoint lacks whole-block receipts, yields one. A read that could still turn
/// out otherwise leaves the swap to be checked again. An endpoint whose identity check passes
/// after the first ones were tried is still asked. A General-only endpoint is never asked.
#[tokio::test(start_paused = true)]
async fn across_reports_a_delivery_only_when_no_later_read_could_verify_it() {
    // The final fill block holds a fill event without evidence, as for a native delivery. 0: it
    // leaves the chain during the read. 1: the chain's only endpoint has no whole-block
    // receipts. 2: one endpoint has none, and another fails to serve them.
    // The identity check of a second endpoint is held until the first, which has no
    // whole-block receipts, was asked for them. 3: the second has none either. 4: the second
    // serves them, and the fill has its token transfer. 5: the check stays held. 6: like 1,
    // with a General-only endpoint that serves them. 7: like 5, with that endpoint too.
    for scenario in 0..8 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, UNPINNED_CHAIN).await;
        let (destination, mut config, server) = across_destination(UNPINNED_CHAIN).await;
        {
            let emitter = Address::repeat_byte(0x5e);
            let mut logs = vec![(
                emitter,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            )];
            if scenario == 4 {
                let payment = Transfer {
                    from: Address::repeat_byte(0x55),
                    to: delivery.receiver,
                    value: terms.output_amount,
                };
                logs.push((terms.output_token, payment.encode_log_data()));
            }
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, emitter, Bytes::new(), logs);
            chain.head = 31;
            chain.reorg_on_receipts = scenario == 0;
            if scenario == 1 {
                chain.receipt_error = Some(-32601);
            }
        }
        let mut servers = vec![server];
        // Per endpoint, the error it answers whole-block receipts with, whether its identity
        // check is held, and whether it is Sensitive.
        let endpoints: &[(Option<i64>, bool, bool)] = match scenario {
            2 => &[(Some(-32601), false, true), (Some(-32000), false, true)],
            3 | 5 => &[(Some(-32601), false, true), (Some(-32601), true, true)],
            4 => &[(Some(-32601), false, true), (None, true, true)],
            6 => &[(Some(-32601), false, true), (None, false, false)],
            7 => &[
                (Some(-32601), false, true),
                (Some(-32601), true, true),
                (None, false, false),
            ],
            _ => &[],
        };
        let general_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if !endpoints.is_empty() {
            let gate = crate::rpc_broker::tests::RpcMockGate {
                request_started: Arc::default(),
                release_response: Arc::default(),
            };
            let asked = Arc::new(tokio::sync::Notify::new());
            let mut urls = Vec::new();
            for &(code, held, sensitive) in endpoints {
                let served = destination.clone();
                let asked = asked.clone();
                let general_requests = general_requests.clone();
                let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(
                    move |request: Value| {
                        if !sensitive {
                            general_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        let result = served.lock().unwrap().respond(&request);
                        if request["method"] != "eth_getBlockReceipts" {
                            return result;
                        }
                        asked.notify_one();
                        code.map_or(result, |code| {
                            json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":code, "message":"unavailable"}})
                        })
                    },
                );
                let (endpoint, server) = if held {
                    crate::rpc_broker::tests::spawn_gated_rpc_mock(
                        responder,
                        Arc::default(),
                        Arc::default(),
                        gate.clone(),
                    )
                    .await
                } else {
                    crate::rpc_broker::tests::spawn_rpc_mock(
                        responder,
                        Arc::default(),
                        Arc::default(),
                    )
                    .await
                };
                let roles = crate::RpcEndpointRoles {
                    general: !sensitive,
                    sensitive,
                    submit: true,
                };
                urls.push((endpoint.into(), crate::RpcTransport::Wallet, roles));
                servers.push(server);
            }
            config.rpc_route = crate::RpcChainRoute::new_with_transports(UNPINNED_CHAIN, urls);
            if scenario >= 3 {
                config.rpc_route = config.rpc_route.with_identity_verification();
            }
            if matches!(scenario, 3 | 4) {
                // Release the held endpoint's requests once the first was asked for receipts.
                servers.push(tokio::spawn(async move {
                    gate.request_started.notified().await;
                    asked.notified().await;
                    loop {
                        gate.release_response.notify_one();
                        gate.request_started.notified().await;
                    }
                }));
            }
        }
        let (url, _, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(|_| {
                across_report("filled", REPORTED_AMOUNT)
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let result = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        let expected = match scenario {
            1 | 3 | 6 => Some(across_reported()),
            4 => Some(across_verified(&destination.lock().unwrap(), &terms, 0)),
            _ => None,
        };
        // A failed read is an error, and so is an endpoint that didn't answer. The swap is
        // polled again.
        assert_eq!(
            result.is_err(),
            matches!(scenario, 2 | 5 | 7),
            "scenario {scenario}"
        );
        assert_eq!(
            general_requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "scenario {scenario}"
        );
        assert_eq!(result.ok().flatten(), expected, "scenario {scenario}");
        let record = fixture.record();
        assert_eq!(bridge_outcome(&record), expected, "scenario {scenario}");
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(expected.is_none()),
            "scenario {scenario}"
        );
        stub.abort();
        for server in servers {
            server.abort();
        }
        fixture.finish().await;
    }
}

/// Nothing on the destination chain backs a delivery Across only reported, and its refund would
/// land on this chain. Routine polling stops at the report, and an explicit status check asks
/// Across again. A report never replaces an outcome already recorded.
#[tokio::test]
async fn an_explicit_check_of_an_across_report_verifies_it_or_finds_its_refund() {
    // The fill block is final and holds a fill event without evidence. 0: the check then finds
    // a fill with the token transfer. 1: Across now reports the deposit expired. 2: Across
    // reports another amount, and nothing else changed. 3: Across reported the deposit expired
    // first, and filled at the check.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, UNPINNED_CHAIN).await;
        let (destination, config, server) = across_destination(UNPINNED_CHAIN).await;
        let emitter = Address::repeat_byte(0x5e);
        let fill = (
            emitter,
            filled_relay(delivery, &terms, 42).encode_log_data(),
        );
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, emitter, Bytes::new(), vec![fill.clone()]);
            chain.head = 31;
        }
        let first = if scenario == 3 { "expired" } else { "filled" };
        let reply = Arc::new(Mutex::new(across_report(first, REPORTED_AMOUNT)));
        let served = reply.clone();
        let (url, lookups, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                served.lock().unwrap().clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let known = if scenario == 3 {
            SwapBridgeOutcome::Refunding
        } else {
            across_reported()
        };
        assert_eq!(
            observe_bridge(&fixture, &clients, &config).await,
            Some(known.clone()),
            "scenario {scenario}"
        );
        // Routine polling asks neither Across nor the destination chain again.
        destination.lock().unwrap().rpc_requests.clear();
        assert_eq!(
            observe_bridge(&fixture, &clients, &config).await,
            Some(known.clone())
        );
        assert_eq!(lookups.lock().unwrap().len(), 1);
        assert_eq!(destination.lock().unwrap().rpc_requests, [] as [Value; 0]);
        match scenario {
            0 => {
                let payment = Transfer {
                    from: Address::repeat_byte(0x55),
                    to: delivery.receiver,
                    value: terms.output_amount,
                };
                destination.lock().unwrap().add_addressed_transaction(
                    30,
                    emitter,
                    Bytes::new(),
                    vec![fill, (terms.output_token, payment.encode_log_data())],
                );
            }
            1 => *reply.lock().unwrap() = across_report("expired", REPORTED_AMOUNT),
            _ => *reply.lock().unwrap() = across_report("filled", REPORTED_AMOUNT + 1),
        }
        let checked = fixture
            .owner
            .check_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(lookups.lock().unwrap().len(), 2);
        let (expected, expected_state) = match scenario {
            0 => (
                across_verified(&destination.lock().unwrap(), &terms, 1),
                SwapOrderState::Done,
            ),
            1 | 3 => (SwapBridgeOutcome::Refunding, SwapOrderState::Refunding),
            _ => (known, SwapOrderState::Done),
        };
        assert_eq!(checked, Some(expected.clone()), "scenario {scenario}");
        let record = fixture.record();
        assert_eq!(
            bridge_outcome(&record),
            Some(expected),
            "scenario {scenario}"
        );
        assert_eq!(state(&record, 0), expected_state, "scenario {scenario}");
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}
