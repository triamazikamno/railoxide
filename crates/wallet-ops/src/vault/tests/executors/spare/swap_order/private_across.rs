use super::*;

// A private Across order's destination stealth account signs a guarded shield on the
// destination chain first. That payload is in its record before the signing-time quote carries
// the handler message out of the wallet and before the swap's own account signs anything. The
// post-hook's deposit then pays the handler with that message, whose fallback follows the
// failure choice. The preview quote names no recipient, message or account.
//
// Each account is new, and set up by this swap, or existing. The first order is signed for the
// accounts the approval binds, each at its current nonce, whichever of them needed setup.
// Once the swap is delivered, both accounts can serve another swap. The destination account is
// then judged again before it signs for that swap: its old shields' nonce must be consumed, its
// earlier shield's note accepted or spent, and its balance of the receiving token zero.
#[tokio::test]
async fn private_across_order_records_the_destination_shield_before_its_deposit_is_signed() {
    use alloy::sol_types::SolValue as _;

    for (source_new, destination_new, on_shield_failure) in [
        (true, true, BridgeShieldFailure::RefundOnOrigin),
        (true, true, BridgeShieldFailure::KeepOnDestination),
        (false, true, BridgeShieldFailure::RefundOnOrigin),
        (true, false, BridgeShieldFailure::KeepOnDestination),
        (false, false, BridgeShieldFailure::RefundOnOrigin),
    ] {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
        let profile = origin_chain.accepted_executor_profile().unwrap();
        let destination_profile = destination_chain.accepted_executor_profile().unwrap();
        let swap_profile = origin_chain.swap_profile().unwrap();
        let spoke_pool = origin_chain.bridge_profile().unwrap().spoke_pool();
        let handler = destination_chain
            .bridge_profile()
            .unwrap()
            .multicall_handler();
        let [owner, destination_owner] = [origin_chain, destination_chain.clone()].map(|chain| {
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        });
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let destination_store =
            ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();

        // Both accounts are claimed for the swap, each as a new account or as one an earlier
        // action set up, and the approval saved with the swap names the destination account as
        // receiver.
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        let mut destination_candidate = broadcaster(destination_profile.delegate());
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let (operation, existing_source) = if source_new {
            (ExecutorOperationId::random().unwrap(), None)
        } else {
            let candidate = broadcaster(profile.delegate());
            let (operation, delegated) = set_up_account(&owner, &store, profile, candidate).await;
            (operation, Some(delegated))
        };
        let (destination_operation, existing_destination) = if destination_new {
            (ExecutorOperationId::random().unwrap(), None)
        } else {
            let (operation, delegated) = set_up_account(
                &destination_owner,
                &destination_store,
                destination_profile,
                destination_candidate.clone(),
            )
            .await;
            (operation, Some(delegated))
        };
        let choice = |new, operation| {
            if new {
                crate::vault::SwapAccountChoice::New(operation)
            } else {
                crate::vault::SwapAccountChoice::Existing(operation)
            }
        };
        let mut approval = setup_approval(
            USDC,
            WETH,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: DESTINATION_CHAIN,
                receiver: Address::ZERO,
                destination_token: POLYGON_WETH,
                surplus: BridgeSurplus::KeepInAccount,
                private: Some(BridgePrivateDelivery { on_shield_failure }),
            }),
        );
        // Only a side that needs setup has a setup fee limit.
        let setup_fee = |new: bool| new.then_some(setup_fee_limit(1_000));
        approval.bounds.source_setup_fee = setup_fee(source_new);
        approval.bounds.destination_setup_fee = setup_fee(destination_new);
        approval.bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
        let prepared = prepare_swap_pair(
            &owner,
            Some(&destination_owner),
            SwapPairPreparation {
                use_id: crate::vault::SwapUseId::first(operation),
                source: choice(source_new, operation),
                destination: Some(choice(destination_new, destination_operation)),
                approval,
                candidate: source_new.then(|| broadcaster(profile.delegate())),
                destination_candidate: destination_new.then_some(destination_candidate),
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
                client: crate::OperationHttpClient::direct_for_tests(),
            },
        )
        .await
        .unwrap();
        let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
            panic!("the approval keeps its Bridge delivery");
        };
        let destination_executor = prepared.destination.as_ref().unwrap().executor();
        assert_eq!(delivery.receiver, destination_executor);
        assert_eq!(
            (
                prepared.origin.requires_setup(),
                prepared.destination.as_ref().unwrap().requires_setup()
            ),
            (source_new, destination_new)
        );
        let destination_record = || {
            destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == destination_operation)
                .unwrap()
        };

        // Before its setup is confirmed, a new destination account can't back an order.
        if destination_new {
            let error = destination_owner
                .delegated_swap_destination(
                    destination_operation,
                    9,
                    1,
                    operation,
                    crate::vault::SwapUseId::first(operation),
                    destination_executor,
                    None,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("destination network"),
                "{error:#}"
            );
            assert_eq!(
                destination_record().issued(),
                [] as [IssuedExecutorPayload; 0]
            );
        }

        // A new account's setup is confirmed now. An existing account keeps the one it had.
        let source_delegated =
            existing_source.unwrap_or_else(|| delegate_setup(&store, operation, profile));
        let fixture = BridgeOrderFixture::planned(
            &view,
            &swap_profile,
            source_delegated,
            prepared.origin.executor(),
            SwapDelivery::Bridge(delivery),
        );
        let executor = fixture.executor;
        let delegated = existing_destination.unwrap_or_else(|| {
            delegate_setup(
                &destination_store,
                destination_operation,
                destination_profile,
            )
        });
        // A reused destination's balance of the receiving token is read from the chain before
        // each of its shields, so the chain holds its delegation and nonce.
        if !destination_new {
            rpc.set_delegated_account(
                destination_executor,
                destination_profile.delegate(),
                delegated.observed().nonce(),
            );
        }
        // A completed destination setup still belongs to its origin swap, even before any
        // shield has been issued. It must not be offered or previewed for an unrelated swap.
        assert_eq!(
            destination_owner.swap_account_candidates().unwrap(),
            [] as [crate::SwapAccountCandidate; 0]
        );
        assert!(
            destination_owner
                .swap_order_preview(destination_operation, true, None)
                .is_err()
        );
        let destination = crate::SwapDestinationSigning {
            owner: &destination_owner,
            delegated,
            authorization: &destination_authorization,
            notes: None,
        };
        let mut review = fixture.review.clone();
        review.set_bridge_for_tests(&crate::SwapBridgeQuote {
            provider: BridgeProvider::Across,
            destination_minimum: U256::from(1_000),
            expected_output: U256::from(1_000),
            expected_output_usd: None,
            fee: Some(U256::ZERO),
            leg: crate::BridgeLegPrice::SameAsset,
            fill_time_sec: None,
            private: Some(crate::SwapPrivateBridgeQuote {
                quoted_output: U256::from(1_050),
                delivery_allowance: U256::from(50),
                destination_shield_fee_bps: crate::RAILGUN_PROTOCOL_FEE_BPS,
                deposit_floor: None,
            }),
        });

        // The quote's output, and whether Across's simulation of the message fails. For each
        // request with a message, the stub records whether the destination account's payload
        // in that message was already in its record, and whether the swap's own account had
        // still signed nothing.
        let quoted = Arc::new(Mutex::new((U256::from(1_005), false)));
        let answer = quoted.clone();
        // An output that answers the next quote only.
        let quoted_once = Arc::new(Mutex::new(None::<U256>));
        let answer_once = quoted_once.clone();
        let durable = Arc::new(Mutex::new(Vec::new()));
        let observed = durable.clone();
        // A destination owner that the stub closes while it answers the quote.
        let ending = Arc::new(Mutex::new(None::<Arc<ExecutorOwner>>));
        let ends = ending.clone();
        let (stub_db, stub_view) = (db.clone(), view.clone());
        let (across_url, across_requests, across_task) = spawn_bridge_stub_with(move |path, _| {
            let url = url::Url::parse(&format!("http://across{path}")).unwrap();
            if let Some((_, message)) = url.query_pairs().find(|(key, _)| key == "message") {
                let message = message.parse::<Bytes>().unwrap();
                let shield = MulticallHandler::Instructions::abi_decode(&message)
                    .unwrap()
                    .calls
                    .swap_remove(1)
                    .callData;
                let issued = |chain_id, operation| {
                    ExecutorStore::new(stub_db.clone(), stub_view.clone(), chain_id)
                        .unwrap()
                        .records()
                        .unwrap()
                        .into_iter()
                        .find(|record| record.operation() == operation)
                        .unwrap()
                        .issued()
                        .to_vec()
                };
                observed.lock().unwrap().push((
                    issued(DESTINATION_CHAIN, destination_operation)
                        .iter()
                        .any(|payload| {
                            payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
                                && *payload.context().calldata() == shield
                        }),
                    issued(1, operation).len() == 1,
                ));
            }
            if let Some(owner) = ends.lock().unwrap().take() {
                owner.close();
            }
            let (output, simulation_fails) = *answer.lock().unwrap();
            let output = answer_once.lock().unwrap().take().unwrap_or(output);
            if simulation_fails {
                let error = json!({
                    "type": "AcrossApiError", "code": "SIMULATION_ERROR", "status": 400,
                    "message": "execution reverted"
                });
                (400, error.to_string())
            } else {
                (200, across_fee_quote(spoke_pool, output, 3 * 60 * 60))
            }
        })
        .await;
        let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
        let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
            db.clone(),
            view.clone(),
            operation,
            swap_profile.settlement(),
        )
        .await;
        let orderbook = CowOrderbookClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            orderbook_url,
            1,
        )
        .unwrap();
        let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
        clients.across = crate::bridge::AcrossClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            across_url,
        )
        .unwrap();
        let bridge_destination = crate::bridge::BridgeDestination {
            destination_token: POLYGON_WETH,
            intermediate: WETH,
            symbol: "WETH".to_owned(),
            same_asset: true,
            near: None,
        };
        let route = crate::SwapBridgeRoute {
            clients: &clients,
            destination: crate::SwapBridgeDestination::Chain {
                destination: &bridge_destination,
                chain: &destination_chain,
            },
        };
        let tokens = crate::settings::EffectiveTokenRegistry {
            tokens: std::collections::BTreeMap::new(),
        };

        // The preview asks Across with the tokens, chains and amount only. Without a cached
        // anchor the delivery allowance is scaled from that quote, so the destination chain's
        // RPC isn't read.
        let (quote_url, _, quote_task) = spawn_quote_stub(
            json!({
                "quote": {
                    "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                    "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                    "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                    "kind": "sell", "partiallyFillable": false
                },
                "expiration": "", "id": 7, "verified": true
            }),
            None,
        )
        .await;
        let previewed = owner
            .review_swap(SwapReviewRequest {
                plan: review.plan().clone(),
                slippage_bps: 50,
                gas_share_bps: GAS_SHARE_BALANCED_BPS,
                valid_for: Duration::from_mins(30),
                orderbook: &CowOrderbookClient::new(
                    OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                    quote_url,
                    1,
                )
                .unwrap(),
                anchor_cache: None,
                token_registry: &tokens,
                bridge: Some(route),
            })
            .await
            .unwrap();
        quote_task.abort();
        assert!(previewed.bridge().unwrap().private.is_some());
        let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
        let path = path.to_lowercase();
        assert!(
            !path.contains("recipient") && !path.contains("message"),
            "{path}"
        );
        for address in [executor, destination_executor, handler] {
            assert!(!path.contains(&format!("{address:x}")), "{path}");
        }
        assert!(durable.lock().unwrap().is_empty());

        // A private order without its destination account is refused.
        assert!(
            fixture
                .issue_with(
                    &owner,
                    &orderbook,
                    route,
                    &review,
                    review.suggested_private_minimum(),
                    None,
                )
                .await
                .is_err()
        );
        fixture.assert_unsigned(&store, &submissions);
        assert_eq!(destination_record().issued().len(), 1);

        // A signing-time quote below the approved destination minimum returns to review, and a
        // message that fails Across's simulation is an error. Neither signs for the swap's own
        // account.
        let issue = || {
            fixture.issue_with(
                &owner,
                &orderbook,
                route,
                &review,
                review.suggested_private_minimum(),
                Some(destination),
            )
        };
        // The first order is signed for the accounts its approval binds. Another setup need or
        // another account returns to review, and neither account signs. An approved new account
        // that has no address yet takes its derived one, which is no change.
        let bound = prepared.approval.accounts.unwrap();
        let mut resetup = bound;
        resetup.source.setup = !source_new;
        let mut replaced = bound;
        replaced.destination = Some(crate::vault::SwapApprovedAccount {
            address: Some(Address::repeat_byte(0xee)),
            setup: destination_new,
        });
        let mut unresolved = bound;
        if source_new {
            unresolved.source.address = None;
        }
        if destination_new {
            unresolved.destination = Some(crate::vault::SwapApprovedAccount {
                address: None,
                setup: true,
            });
        }
        for (accounts, changed) in [(resetup, true), (replaced, true), (unresolved, false)] {
            let approval = SwapApproval {
                accounts: Some(accounts),
                ..prepared.approval.clone()
            };
            owner
                .record_swap_approval(
                    operation,
                    crate::vault::SwapUseId::first(operation),
                    approval,
                )
                .unwrap();
            if changed {
                assert_eq!(
                    issue().await.unwrap(),
                    SwapOrderOutcome::ReviewRequired(SwapReviewChange::Accounts)
                );
                fixture.assert_unsigned(&store, &submissions);
                assert_eq!(destination_record().issued().len(), 1);
            }
        }

        *quoted.lock().unwrap() = (U256::from(999), false);
        assert_eq!(
            issue().await.unwrap(),
            SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
                approved: U256::from(1_000),
                current: U256::from(999),
            })
        );
        fixture.assert_unsigned(&store, &submissions);
        *quoted.lock().unwrap() = (U256::from(1_005), true);
        let error = issue().await.unwrap_err();
        assert!(
            error.to_string().contains("fails in its simulation"),
            "{error:#}"
        );
        fixture.assert_unsigned(&store, &submissions);

        // The destination session is needed until the order is signed. One that ends while the
        // quote is pending stops the order.
        *quoted.lock().unwrap() = (U256::from(1_005), false);
        let ended = Arc::new(
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                destination_chain.clone(),
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        *ending.lock().unwrap() = Some(ended.clone());
        assert!(
            fixture
                .issue_with(
                    &owner,
                    &orderbook,
                    route,
                    &review,
                    review.suggested_private_minimum(),
                    Some(crate::SwapDestinationSigning {
                        owner: &ended,
                        ..destination
                    }),
                )
                .await
                .is_err(),
            "the ended destination session stops the order"
        );
        fixture.assert_unsigned(&store, &submissions);

        // The review allows gas of about 3% of the quote. Its signing quote is one unit short
        // of the approved minimum, and the deposit that delivers 1,000 again costs less than a
        // fifth of that gas, so Across is asked once more for the raised deposit and the order
        // is signed without a review.
        let plan = review.plan().clone();
        let gas_price_wei = 100_000_000_000_000_000 / u128::from(plan.hook_gas_estimate());
        let mut cushioned = BridgeOrderFixture::review_at(
            plan,
            &swap_profile,
            BridgeProvider::Across,
            gas_price_wei,
            1_000,
        );
        cushioned.set_bridge_for_tests(review.bridge().unwrap());
        let reviewed_minimum = cushioned.suggested_private_minimum();
        let reviewed_buy = cushioned.buy_amount_for(reviewed_minimum).unwrap();
        *quoted_once.lock().unwrap() = Some(U256::from(999));
        let outcome = fixture
            .issue_with(
                &owner,
                &orderbook,
                route,
                &cushioned,
                reviewed_minimum,
                Some(destination),
            )
            .await
            .unwrap();
        let SwapOrderOutcome::Submitted { uid } = outcome else {
            panic!("the order is submitted");
        };
        assert_eq!(
            *durable.lock().unwrap(),
            [(true, true); 5],
            "each message's shield payload is durable before the request, and nothing of the swap's own account is signed yet"
        );

        // The post-hook guards, approves and deposits for the handler, with the message.
        let submitted = submissions.lock().unwrap().clone();
        let [(persisted, body)] = submitted.as_slice() else {
            panic!("one order request");
        };
        assert!(*persisted, "the terms are durable before the order request");
        let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
            .unwrap()
            .metadata
            .hooks;
        let calls = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data)
            .unwrap()
            ._calls;
        assert_eq!(
            calls.iter().map(|call| call.to).collect::<Vec<_>>(),
            [executor, WETH, spoke_pool]
        );
        let deposit = SpokePool::depositV3Call::abi_decode(&calls[2].data).unwrap();
        assert_eq!((deposit.depositor, deposit.recipient), (executor, handler));
        assert_eq!(deposit.outputAmount, U256::from(1_000));
        let instructions = MulticallHandler::Instructions::abi_decode(&deposit.message).unwrap();
        let shield_calldata = instructions.calls[1].callData.clone();
        let fallback = match on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => None,
            BridgeShieldFailure::KeepOnDestination => Some(destination_executor),
        };
        let message = private_delivery_message(
            handler,
            POLYGON_WETH,
            destination_executor,
            shield_calldata.clone(),
            fallback,
        );
        assert_eq!(deposit.message, message);

        // The signing-time quote named the handler and carried that message.
        let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
        let url = url::Url::parse(&format!("http://across{path}")).unwrap();
        let sent = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        assert_eq!(sent("recipient").parse::<Address>().unwrap(), handler);
        assert_eq!(sent("message").parse::<Bytes>().unwrap(), message);
        // It asked for the raised deposit, which the order signs for the approved minimum.
        let raised_buy = sent("amount").parse::<U256>().unwrap();
        assert!(raised_buy > reviewed_buy);

        // The destination account's record holds that payload at its current nonce: the guard
        // for the approved destination minimum, then a full-balance shield of the token.
        let payload = destination_record()
            .issued()
            .iter()
            .find(|payload| *payload.context().calldata() == shield_calldata)
            .cloned()
            .unwrap();
        assert_eq!(
            (payload.purpose(), payload.nonce()),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                delegated.observed().nonce()
            )
        );
        let shield = RelayAdapt7702::multicallCall::abi_decode(&shield_calldata).unwrap();
        assert!(shield._requireSuccess);
        assert_eq!(shield._nonce, delegated.observed().nonce());
        assert_eq!(
            shield._calls.iter().map(|call| call.to).collect::<Vec<_>>(),
            [destination_executor; 2]
        );
        let guard = transferCall::abi_decode(&shield._calls[0].data).unwrap();
        assert_eq!(
            (guard._transfers[0].to, guard._transfers[0].value),
            (destination_executor, U256::from(1_000))
        );
        let shielded = shieldCall::abi_decode(&shield._calls[1].data).unwrap();
        let preimage = &shielded._shieldRequests[0].preimage;
        assert_eq!(preimage.token.tokenAddress, POLYGON_WETH);
        assert!(preimage.value.is_zero());

        // The swap's own account signed its hooks at its current nonce and the one after.
        let source_nonce = source_delegated.observed().nonce();
        let hooks = fixture.record(&store);
        for (purpose, nonce) in [
            (ExecutorPayloadPurpose::SwapPreHook, source_nonce),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                source_nonce + U256::ONE,
            ),
        ] {
            assert!(
                hooks
                    .issued()
                    .iter()
                    .any(|payload| payload.purpose() == purpose && payload.nonce() == nonce)
            );
        }

        // A retry of the same use signs its own shield at the same nonce, and the signature
        // the order's deposit carries stays recorded.
        let retried = destination_owner
            .issue_swap_destination_shield(
                delegated,
                crate::vault::SwapUseId::first(operation),
                POLYGON_WETH,
                U256::from(1_001),
                &destination_authorization,
                None,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_ne!(retried, shield_calldata);
        let reissued = destination_record();
        for calldata in [&shield_calldata, &retried] {
            assert!(reissued.issued().iter().any(|payload| {
                payload.context().calldata() == calldata
                    && payload.nonce() == delegated.observed().nonce()
            }));
        }

        // The order keeps the signed recipient and message hash, and the private terms.
        let saved = fixture.record(&store).swap().unwrap().orders()[0].clone();
        assert_eq!(saved.uid(), uid);
        let Some(BridgeOrderTerms::Across(terms)) = saved.bridge().cloned() else {
            panic!("an Across order keeps its deposit terms");
        };
        assert_eq!(
            (
                terms.input_amount,
                saved.bounds().buy_amount,
                saved.bounds().destination_minimum
            ),
            (raised_buy, raised_buy, Some(U256::from(1_000)))
        );
        assert_eq!(
            (terms.recipient, terms.message_hash),
            (Some(handler), Some(alloy::primitives::keccak256(&message)))
        );
        assert_eq!(
            (
                saved.bounds().destination_shield_fee_bps,
                saved.bounds().delivery_allowance,
                saved.bounds().destination_setup_fee,
            ),
            (
                Some(crate::RAILGUN_PROTOCOL_FEE_BPS),
                Some(U256::from(50)),
                setup_fee(destination_new),
            )
        );

        // The order trades and hands off, and its fill on the destination chain runs the
        // shield. The destination account's record takes that outcome from its origin swap.
        let (fill_block, fill) = (
            BlockNumHash::new(15, B256::repeat_byte(15)),
            B256::repeat_byte(0x61),
        );
        let traded = crate::vault::SwapObservation {
            block: BlockNumHash::new(13, B256::repeat_byte(13)),
            transaction_hash: Some(B256::repeat_byte(0x50)),
        };
        store
            .record_swap_settlement(
                operation,
                uid,
                &crate::vault::SwapSettlementEvidence {
                    traded,
                    amounts: crate::vault::SwapTradeAmounts {
                        sell_amount: U256::from(997_500),
                        buy_amount: U256::from(300_000_000_000_000_000_u64),
                        fee_amount: U256::ZERO,
                        settlement_gas_used: None,
                        settlement_effective_gas_price: None,
                        executed_fee: None,
                        executed_fee_token: None,
                    },
                    credit: None,
                    handoff: Some(crate::vault::SwapBridgeHandoff {
                        observation: traded,
                        deposit_id: Some(U256::from(77)),
                    }),
                    delivery: uid,
                },
            )
            .unwrap();
        store
            .record_swap_bridge_outcome(
                operation,
                uid,
                crate::vault::SwapBridgeOutcome::DeliveredVerified {
                    block: fill_block,
                    transaction_hash: fill,
                    output_amount: U256::from(1_000),
                    shielded: true,
                },
            )
            .unwrap();
        assert!(destination_store.reconcile_swap_destinations().unwrap());

        // Another swap names both accounts. While the delivered swap's shields are still at
        // the account's recorded nonce, the destination takes no other swap and signs nothing.
        let second = crate::vault::SwapUseId::random().unwrap();
        let mut second_approval = prepared.approval.clone();
        let accounts = second_approval.accounts.as_mut().unwrap();
        accounts.source.setup = false;
        accounts.destination.as_mut().unwrap().setup = false;
        let claim = || {
            owner.claim_swap_use(
                Some(&destination_owner),
                crate::SwapUseClaim {
                    id: second,
                    source: crate::vault::SwapAccountChoice::Existing(operation),
                    approval: second_approval.clone(),
                    destination: Some(crate::vault::SwapAccountChoice::Existing(
                        destination_operation,
                    )),
                },
            )
        };
        let issued_count = || destination_record().issued().len();
        let issued_before = issued_count();
        assert!(claim().is_err());
        assert_eq!(
            destination_owner
                .swap_account_refusal(
                    destination_operation,
                    SwapAccountRole::Destination {
                        token: POLYGON_WETH
                    },
                )
                .unwrap(),
            Some(SwapAccountRefusal::UnfinishedWork)
        );
        destination_store
            .record_account_read(
                destination_operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(20, B256::repeat_byte(20)),
                    U256::from(2),
                ),
            )
            .unwrap();

        // The account now has its delegation's code and execution nonce 2 on the chain.
        rpc.state.head.store(40, Ordering::Relaxed);
        rpc.set_delegated_account(
            destination_executor,
            destination_profile.delegate(),
            U256::from(2),
        );
        let answer_call = |contract, word: Option<u64>| {
            rpc.state
                .calls
                .lock()
                .unwrap()
                .insert(contract, word.map(U256::from));
        };
        // The settled former destination is offered in either role, and is admitted to place
        // orders from its current state alone.
        let offered = |candidates: Vec<crate::SwapAccountCandidate>| {
            candidates
                .iter()
                .any(|candidate| candidate.operation() == destination_operation)
        };
        assert!(offered(
            destination_owner.swap_account_candidates().unwrap()
        ));
        assert!(offered(
            destination_owner
                .swap_destination_candidates(POLYGON_WETH)
                .unwrap()
        ));
        let sourced = destination_owner
            .reuse_swap_account(
                destination_operation,
                39,
                SwapAccountRole::Source,
                SwapAccountUse::New,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(sourced.expected_pre_hook_nonce(), U256::from(2));

        // The second swap claims both accounts. Admission of its destination reads the
        // earlier shield's note from the wallet's local notes: none, a blocked one and one
        // without a verdict each refuse, despite the consumed nonce and an empty balance.
        claim().unwrap();
        let notes = ShieldNotes::default();
        let shield_note = |status: Option<crate::PoiStatus>, spent: bool| {
            let mut utxo = Utxo::new(
                broadcaster_core::notes::Note::new_change(
                    view.scan_keys().master_public_key,
                    POLYGON_WETH,
                    U256::from(1_000),
                    [9; 16],
                ),
                0,
                7,
                UtxoSource {
                    tx_hash: fill,
                    block_number: fill_block.number,
                    block_timestamp: 0,
                },
                UtxoCommitmentKind::Shield,
            );
            if let Some(status) = status {
                for list in poi::poi::default_active_poi_list_keys() {
                    utxo.poi.statuses.insert(list, status);
                }
            }
            let mut note = crate::WalletUtxo::new(utxo);
            if spent {
                note.spent = Some(UtxoSource {
                    tx_hash: B256::repeat_byte(0x62),
                    block_number: fill_block.number + 1,
                    block_timestamp: 0,
                });
            }
            note
        };
        let hold = |note| *notes.0.lock().unwrap() = vec![note];
        let admit = || {
            destination_owner.delegated_swap_destination(
                destination_operation,
                39,
                1,
                operation,
                second,
                destination_executor,
                Some(&notes),
                crate::OperationHttpClient::direct_for_tests(),
            )
        };
        let refusal = |error: eyre::Report| {
            *error
                .downcast_ref::<SwapAccountRefusal>()
                .unwrap_or_else(|| panic!("{error:#}"))
        };
        let transaction_hash = fill;
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldUnknown { transaction_hash }
        );
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), false));
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldBlocked { transaction_hash }
        );
        hold(shield_note(None, false));
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldPending { transaction_hash }
        );
        // A spent note needs no refund, whatever its verdict was, and an accepted one admits.
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), true));
        admit().await.unwrap();
        hold(shield_note(Some(crate::PoiStatus::Valid), false));
        let reused = admit().await.unwrap();
        assert_eq!(reused.observed().nonce(), U256::from(2));

        // The shield is judged again when it is issued. A verdict that changed since
        // admission, a balance of the receiving token and a balance that can't be read each
        // issue nothing.
        let issue_shield = || {
            destination_owner.issue_swap_destination_shield(
                reused,
                second,
                POLYGON_WETH,
                U256::from(1_000),
                &destination_authorization,
                Some(&notes),
                crate::OperationHttpClient::direct_for_tests(),
            )
        };
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), false));
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldBlocked { transaction_hash }
        );
        hold(shield_note(Some(crate::PoiStatus::Valid), false));
        answer_call(POLYGON_WETH, Some(5));
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::ReceivingBalance
        );
        answer_call(POLYGON_WETH, None);
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::ReceivingBalanceUnknown
        );
        assert_eq!(issued_count(), issued_before);

        // Native currency in the account doesn't fail the balance rule. The shield is signed
        // at the account's current nonce, and the first swap's shields stay recorded.
        answer_call(POLYGON_WETH, Some(0));
        rpc.state.native_balance.store(7, Ordering::Relaxed);
        let reissued = issue_shield().await.unwrap();
        let record = destination_record();
        assert_eq!(record.issued().len(), issued_before + 1);
        let payload = record.issued().last().unwrap();
        assert_eq!(
            (
                payload.purpose(),
                payload.nonce(),
                payload.context().calldata()
            ),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                U256::from(2),
                &reissued
            )
        );

        across_task.abort();
        orderbook_task.abort();
        owner.shutdown().await;
        destination_owner.shutdown().await;
        drop((owner, destination_owner));
        drop((store, destination_store));
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
