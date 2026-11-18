use super::*;

/// One pass of the observation loop over the pages of the swap `operation`, a private Bridge
/// swap's destination page included. Returns the routes its RPC requests went over, and
/// whether each page read a destination account.
fn observation_routes(
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    operation: ExecutorOperationId,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> (BTreeSet<String>, Vec<bool>) {
    let (owner, pages) = cx.update(|_, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            swaps.reload_destinations(cx);
            swaps.track_records();
            let (_, owner, pages) = swaps.next_observations(cx).expect("pages to observe");
            let pages = pages
                .into_iter()
                .filter(|page| {
                    page.operation == operation
                        || page
                            .destination
                            .as_ref()
                            .is_some_and(|destination| destination.origin == operation)
                })
                .collect::<Vec<_>>();
            (owner, pages)
        })
    });
    let read = pages
        .iter()
        .map(|page| page.destination.is_some())
        .collect();
    let from = stubs.request_count();
    let results = runtime.block_on(observe_pages(owner, pages));
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.apply_observations(results, window, cx);
        });
    });
    let routes = stubs.routes_since(from, "/rpc").into_iter().flatten();
    (routes.collect(), read)
}

/// Sets the synced head of each of `chains`, so their confirmed blocks are known.
fn sync_heads(root: &Entity<WalletRoot>, chains: &[u64], cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        root.update(cx, |root, _| {
            for chain in chains {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(chain)
                else {
                    panic!("ready chain {chain}");
                };
                sync_tip.head_block = Some(10_000);
            }
        });
    });
}

/// Each tracked swap's account is read over a route of its own, which its later pages keep.
/// Two swaps never share one.
#[gpui::test]
fn two_tracked_swaps_are_read_over_routes_of_their_own(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            let issued = pending_setup(executors, operation);
            let other = ExecutorOperationId::random().unwrap();
            executors
                .reserve_with_swap_approval(
                    other,
                    issued.delegate(),
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
                .bind_address(other, Address::repeat_byte(0x35))
                .unwrap();
            record_setup(
                executors,
                other,
                issued.delegate(),
                B256::repeat_byte(5),
                Vec::new(),
            );
            cx.update(|_, cx| root.update(cx, |root, _| root.http = tagged_operation_http()));
            sync_heads(root, &[1], cx);

            let (first, _) = observation_routes(swaps, &stubs, operation, runtime, cx);
            let (second, _) = observation_routes(swaps, &stubs, other, runtime, cx);
            assert_eq!(first.len(), 1, "{first:?}");
            assert_eq!(second.len(), 1, "{second:?}");
            assert_ne!(first, second, "two swaps never share a route");
            let (again, _) = observation_routes(swaps, &stubs, operation, runtime, cx);
            assert_eq!(again, first, "a swap's later pages keep its route");
        },
    );
}

/// A private Bridge swap's setup pages read its own account and its destination account over
/// the swap's one route.
#[gpui::test]
fn a_private_bridge_swaps_origin_and_destination_pages_share_its_route(cx: &mut TestAppContext) {
    use wallet_ops::vault::{SwapApprovalTokens, SwapDestinationClaim, SwapPairClaim};
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, _, runtime, store, cx| {
            cx.update(|_, cx| root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137)));
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon_store, _) = reusable_polygon_account(root, cx);
            let (origin_delegate, polygon_delegate) = root.read_with(cx, |root, _| {
                let delegate = |chain_id| {
                    root.effective_chain_configs
                        .get(chain_id)
                        .unwrap()
                        .accepted_executor_profile()
                        .unwrap()
                        .delegate()
                };
                (delegate(1), delegate(137))
            });
            // Both accounts are new, and each setup was sent.
            let source = ExecutorOperationId::random().unwrap();
            let destination = ExecutorOperationId::random().unwrap();
            let receiver = Address::repeat_byte(0x61);
            let approval = SwapApproval {
                delivery: SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver,
                    destination_token: STUB_POLYGON_USDT,
                    surplus: BridgeSurplus::Reshield,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
                    }),
                }),
                tokens: Some(SwapApprovalTokens {
                    sell: Address::repeat_byte(1),
                    buy: Address::repeat_byte(2),
                }),
                accounts: Some(SwapApprovedAccounts {
                    source: SwapApprovedAccount {
                        address: None,
                        setup: true,
                    },
                    destination: Some(SwapApprovedAccount {
                        address: None,
                        setup: true,
                    }),
                }),
                ..test_approval()
            };
            executors
                .claim_swap_pair(SwapPairClaim {
                    id: SwapUseId::first(source),
                    source: SwapAccountChoice::New(source),
                    delegate: origin_delegate,
                    purpose_summary: Some("Private swap".into()),
                    assets: Vec::new(),
                    approval,
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::New(destination),
                        delegate: polygon_delegate,
                        destination_token: STUB_POLYGON_USDT,
                    }),
                })
                .unwrap();
            executors
                .bind_address(source, Address::repeat_byte(0x33))
                .unwrap();
            polygon_store.bind_address(destination, receiver).unwrap();
            record_setup(
                executors,
                source,
                origin_delegate,
                B256::repeat_byte(0x43),
                Vec::new(),
            );
            record_setup(
                &polygon_store,
                destination,
                polygon_delegate,
                B256::repeat_byte(0x44),
                Vec::new(),
            );
            cx.update(|_, cx| root.update(cx, |root, _| root.http = tagged_operation_http()));
            sync_heads(root, &[1, 137], cx);

            let (routes, read) = observation_routes(swaps, &stubs, source, runtime, cx);
            assert_eq!(
                read,
                [false, true],
                "the swap's own page and its destination's"
            );
            assert_eq!(routes.len(), 1, "{routes:?}");
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}
