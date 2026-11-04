use std::collections::BTreeSet;

use super::*;
use crate::root::wallet_activity::{
    ACTIVITY_KINDS, ActivityChip, ActivityClearKind, ActivityData, ActivityFacet, ActivityFilter,
    ActivityLabels, ActivityRowId, ActivityRowsInput, ActivityStandIn, ActivityStatusChoice,
    ActivitySuggestionAction, activity_suggestions, build_activity_rows, row_matches,
};
use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_component::IndexPath;
use wallet_ops::vault::{
    ActivityClear, ActivityClearAge, ActivityEntry, ActivityEntryId, ActivityKind, ActivityMarkers,
    ActivityMatch, ActivityNetwork, ActivityOwner, ActivitySource, SwapUseId,
};
use wallet_ops::{ActivityStatus, ActivityUtxoClassification, SwapRow};

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A wallet view on chain 1 whose Activity is `stand_in`, so nothing reads the vault's journal.
fn activity_root<'a>(
    path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
    stand_in: ActivityStandIn,
    cx: &'a mut TestAppContext,
) -> (Entity<WalletRoot>, &'a mut VisualTestContext) {
    let (root, cx) = wallet_view_root(path, runtime, cx);
    root.update(cx, |root, cx| {
        root.install_wallet_activity_stand_in(stand_in, cx);
    });
    (root, cx)
}

fn close(root: Entity<WalletRoot>, cx: &mut VisualTestContext, path: &std::path::Path) {
    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

fn entry(network: ActivityNetwork, kind: ActivityKind, created_at: u64) -> ActivityEntry {
    ActivityEntry {
        id: ActivityEntryId::random().unwrap(),
        owner: ActivityOwner::Wallet("preview".to_owned()),
        network,
        kind,
        source: ActivitySource::Wallet,
        created_at,
        legs: Vec::new(),
        counterparty: None,
        fee: None,
        gas_payer: None,
        match_key: ActivityMatch::Hashes,
        attempts: Vec::new(),
        relay_timed_out: false,
        cleared: false,
    }
}

/// A completed swap on chain 1 that started at `started_at`.
fn swap(started_at: u64) -> SwapRow {
    SwapRow {
        id: SwapUseId::random().unwrap(),
        network: ActivityNetwork::Evm(1),
        started_at,
        legs: Vec::new(),
        hashes: Vec::new(),
        status: ActivityStatus::Confirmed(None),
        in_flight: false,
        recipient: None,
    }
}

/// A note received in block 11 at `1_700_000_011` from a transaction that spent nothing.
fn received_note() -> UtxoOutput {
    utxo_output_with_hashes(
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        "1000000",
        false,
        "0x3333333333333333333333333333333333333333333333333333333333333333",
        None,
    )
}

/// The rows the Activity list shows, in order.
fn row_ids(root: &Entity<WalletRoot>, cx: &VisualTestContext) -> Vec<ActivityRowId> {
    root.read_with(cx, |root, cx| {
        root.wallet_activity
            .list
            .read(cx)
            .delegate()
            .rows()
            .iter()
            .map(|row| row.id)
            .collect()
    })
}

/// The row the pane beside the list shows, and the list's own selected index.
fn selection(
    root: &Entity<WalletRoot>,
    cx: &VisualTestContext,
) -> (Option<ActivityRowId>, Option<IndexPath>) {
    root.read_with(cx, |root, cx| {
        let list = root.wallet_activity.list.read(cx);
        (
            list.delegate().selected_row().map(|row| row.id),
            list.selected_index(),
        )
    })
}

#[gpui::test]
fn activity_pins_running_entries_lists_newest_first_and_follows_the_chain(cx: &mut TestAppContext) {
    const BASE: u64 = 1_700_000_000;
    let chain = ActivityNetwork::Evm(1);
    let running = entry(chain, ActivityKind::PublicSend, BASE + 1);
    let older = entry(chain, ActivityKind::PrivateSend, BASE + 5);
    let newer = entry(chain, ActivityKind::Shield, BASE + 20);
    let other_chain = entry(
        ActivityNetwork::Evm(10),
        ActivityKind::PublicSend,
        BASE + 50,
    );
    let stub = ActivityEntry {
        cleared: true,
        ..entry(chain, ActivityKind::Shield, BASE + 30)
    };
    let received = ActivityRowId::Received(received_note().source_tx_hash.parse().unwrap());
    let swapped = swap(BASE + 15);
    let cleared_swap = swap(BASE + 16);
    let stand_in = ActivityStandIn {
        data: ActivityData {
            entries: vec![
                older.clone(),
                running.clone(),
                stub,
                newer.clone(),
                other_chain.clone(),
            ],
            markers: ActivityMarkers {
                hidden_swap_uses: BTreeSet::from([cleared_swap.id]),
                ..ActivityMarkers::default()
            },
            records: Vec::new(),
            swaps: vec![cleared_swap, swapped.clone()],
        },
        utxos: vec![received_note()],
        running: BTreeSet::from([running.id]),
        selected: None,
        chain_id: 1,
    };
    let path = temp_wallet_db_root("activity-order");
    let runtime = test_runtime();
    let _entered = runtime.enter();
    let (root, cx) = activity_root(&path, &runtime, stand_in, cx);

    // The running entry is pinned above newer ones; the rest are newest first, the swap by its
    // start and the received row (block time BASE + 11) between the entries, and the cleared
    // stub and swap are hidden.
    assert_eq!(
        row_ids(&root, cx),
        vec![
            ActivityRowId::Entry(running.id),
            ActivityRowId::Entry(newer.id),
            ActivityRowId::Swap(swapped.id),
            received,
            ActivityRowId::Entry(older.id),
        ]
    );

    // Another chain lists only its own entries and swaps. It has no Railgun, so no received
    // rows.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let mut public_only = root.effective_chain_configs.get(1).unwrap().clone();
            public_only.chain_id = 10;
            public_only.railgun = None;
            public_only.rpc_route = wallet_ops::RpcChainRoute::new(10, Vec::<reqwest::Url>::new());
            root.effective_chain_configs = root
                .effective_chain_configs
                .values()
                .cloned()
                .chain([public_only])
                .collect();
            root.select_chain(10, window, cx);
        });
    });
    assert_eq!(
        row_ids(&root, cx),
        vec![ActivityRowId::Entry(other_chain.id)]
    );
    close(root, cx, &path);
}

#[gpui::test]
fn activity_tab_shows_unread_received_funds_until_opened(cx: &mut TestAppContext) {
    let stand_in = ActivityStandIn {
        data: ActivityData {
            entries: Vec::new(),
            markers: ActivityMarkers {
                // The note arrived in block 11, after the last-seen block.
                last_seen: BTreeMap::from([(ActivityNetwork::Evm(1), 5)]),
                ..ActivityMarkers::default()
            },
            ..ActivityData::default()
        },
        utxos: vec![received_note()],
        running: BTreeSet::default(),
        selected: None,
        chain_id: 1,
    };
    let path = temp_wallet_db_root("activity-unread");
    let runtime = test_runtime();
    let _entered = runtime.enter();
    let (root, cx) = activity_root(&path, &runtime, stand_in, cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        root.read_with(cx, |root, _| root.active_wallet_tab),
        WalletTab::Private
    );
    assert!(cx.debug_bounds("wallet-tab-activity-unread").is_some());

    root.update(cx, |root, cx| {
        root.select_wallet_tab(WalletTab::Activity, cx);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("wallet-tab-activity-unread").is_none());
    root.read_with(cx, |root, _| {
        assert!(!root.wallet_activity.unread());
        assert_eq!(
            root.wallet_activity.last_seen(ActivityNetwork::Evm(1)),
            Some(11)
        );
    });
    // Leaving and coming back shows nothing unread.
    root.update(cx, |root, cx| root.select_wallet_tab(WalletTab::Public, cx));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("wallet-tab-activity-unread").is_none());
    close(root, cx, &path);
}

#[gpui::test]
fn activity_pane_shows_the_selected_row_while_the_search_keeps_it(cx: &mut TestAppContext) {
    let chain = ActivityNetwork::Evm(1);
    let send = entry(chain, ActivityKind::PrivateSend, 1_700_000_005);
    let shield = entry(chain, ActivityKind::Shield, 1_700_000_020);
    let stand_in = ActivityStandIn {
        data: ActivityData {
            entries: vec![send.clone(), shield.clone()],
            ..ActivityData::default()
        },
        chain_id: 1,
        ..ActivityStandIn::default()
    };
    let path = temp_wallet_db_root("activity-selection");
    let runtime = test_runtime();
    let _entered = runtime.enter();
    let (root, cx) = activity_root(&path, &runtime, stand_in.clone(), cx);
    assert_eq!(selection(&root, cx), (None, None));

    // The send is the second row, under the shield.
    cx.update(|window, cx| {
        let list = root.read(cx).wallet_activity.list.clone();
        list.update(cx, |list, cx| {
            list.set_selected_index(Some(IndexPath::new(1)), window, cx);
        });
    });
    let send_id = ActivityRowId::Entry(send.id);
    assert_eq!(
        selection(&root, cx),
        (Some(send_id), Some(IndexPath::new(1)))
    );

    // A search that keeps the send keeps it selected, now as the first row.
    root.update(cx, |root, cx| {
        root.set_wallet_activity_query("  Private ", cx);
    });
    cx.run_until_parked();
    assert_eq!(row_ids(&root, cx), [send_id]);
    assert_eq!(
        selection(&root, cx),
        (Some(send_id), Some(IndexPath::new(0)))
    );

    // One typed into the field that hides it clears the selection.
    root.update(cx, |root, cx| {
        root.select_wallet_tab(WalletTab::Activity, cx);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let input = root.read_with(cx, |root, _| root.wallet_activity.search_input());
    cx.update(|window, cx| {
        input.read(cx).focus_handle(cx).focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_input("shield");
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(input.read(cx).value(), "shield"));
    let shield_id = ActivityRowId::Entry(shield.id);
    assert_eq!(row_ids(&root, cx), [shield_id]);
    assert_eq!(selection(&root, cx), (None, None));

    // Running the search records it for this wallet session only. The next session starts
    // with an empty field that shows every row.
    root.update(cx, super::super::WalletRoot::search_wallet_activity_text);
    root.read_with(cx, |root, _| {
        assert_eq!(root.wallet_activity.recent_searches().len(), 1);
    });
    root.update(cx, |root, cx| {
        root.install_wallet_activity_stand_in(stand_in, cx);
    });
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        assert_eq!(root.wallet_activity.recent_searches().len(), 0);
    });
    cx.update(|_, cx| assert_eq!(input.read(cx).value(), ""));
    assert_eq!(row_ids(&root, cx), [shield_id, send_id]);
    close(root, cx, &path);
}

#[test]
fn activity_search_matches_text_and_chips_and_suggests_them() {
    use alloy::primitives::{Address, B256};
    use wallet_ops::vault::{
        ActivityAsset, ActivityAttempt, ActivityCounterparty, ActivityDirection, ActivityLeg,
        ActivityStep, PublicAddressBookEntry,
    };

    let chain = ActivityNetwork::Evm(1);
    let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        .parse()
        .unwrap();
    let weth: Address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
        .parse()
        .unwrap();
    let friend = Address::repeat_byte(0x4e);
    // Every row moves 25_000_000 units, which no address or hash here contains.
    let moving = |kind, token, hash| ActivityEntry {
        legs: vec![ActivityLeg {
            asset: ActivityAsset::Token(token),
            amount: U256::from(25_000_000_u64),
            direction: ActivityDirection::Out,
        }],
        attempts: vec![ActivityAttempt {
            hash: B256::repeat_byte(hash),
            step: ActivityStep::Main,
            outcome: None,
        }],
        ..entry(chain, kind, 1_700_000_000)
    };
    let send = ActivityEntry {
        counterparty: Some(ActivityCounterparty::Evm(friend)),
        ..moving(ActivityKind::PublicSend, usdc, 0xab)
    };
    // A swap that pays a saved address, found like a send to it.
    let bob = Address::repeat_byte(0x5b);
    let paid = SwapRow {
        recipient: Some(wallet_ops::SwapRowRecipient::Evm {
            address: bob,
            chain_id: 1,
        }),
        ..swap(1_700_000_000)
    };
    let shield = moving(ActivityKind::Shield, weth, 0xcd);
    let swap = moving(ActivityKind::Swap, weth, 0xef);
    let book = [
        PublicAddressBookEntry {
            entry_uuid: "friend".to_owned(),
            label: "Alice Smith".to_owned(),
            address: friend,
            display_order: 0,
        },
        PublicAddressBookEntry {
            entry_uuid: "bob".to_owned(),
            label: "Bob".to_owned(),
            address: bob,
            display_order: 1,
        },
    ];
    let data = ActivityData {
        entries: vec![send.clone(), shield.clone(), swap.clone()],
        swaps: vec![paid.clone()],
        ..ActivityData::default()
    };
    let running = BTreeSet::new();
    let input = ActivityRowsInput {
        network: chain,
        data: &data,
        utxos: &[],
        running: &running,
        labels: ActivityLabels {
            private: &[],
            public: &book,
            non_evm: &[],
            accounts: &[],
        },
        tokens: None,
        native: ("ETH", 18),
        prices: None,
    };
    let rows = build_activity_rows(&input).rows;
    let shown = |filter: &ActivityFilter| -> Vec<ActivityRowId> {
        rows.iter()
            .filter(|row| row_matches(row, filter))
            .map(|row| row.id)
            .collect()
    };
    let searching = |query: &str| {
        let mut filter = ActivityFilter::default();
        filter.set_query(query);
        filter
    };
    let id = |entry: &ActivityEntry| ActivityRowId::Entry(entry.id);
    let none: [ActivityRowId; 0] = [];

    assert_eq!(shown(&searching("ABABAB")), [id(&send)]);
    assert_eq!(shown(&searching("alice")), [id(&send)]);
    assert_eq!(shown(&searching("WETH")), [id(&shield), id(&swap)]);
    assert_eq!(shown(&searching("25")), none);
    let paid_id = ActivityRowId::Swap(paid.id);
    assert_eq!(shown(&searching("bob")), [paid_id]);
    let mut with_bob = ActivityFilter::default();
    with_bob.chips = vec![ActivityChip::With(bob.to_checksum(None).to_lowercase())];
    assert_eq!(shown(&with_bob), [paid_id]);

    // Chips of one filter OR together; a chip of another filter and the text AND with them.
    let chipped = |query, token: Option<Address>| {
        let mut filter = searching(query);
        filter.chips = [ActivityKind::Swap, ActivityKind::PublicSend]
            .map(|kind| ActivityChip::Kind(ActivityClearKind::Entry(kind)))
            .into_iter()
            .chain(token.map(|token| ActivityChip::Token(ActivityAsset::Token(token))))
            .collect();
        filter
    };
    assert_eq!(shown(&chipped("", None)), [id(&send), id(&swap), paid_id]);
    assert_eq!(shown(&chipped("", Some(weth))), [id(&swap)]);
    assert_eq!(shown(&chipped("efef", Some(weth))), [id(&swap)]);
    assert_eq!(shown(&chipped("cdcd", Some(weth))), none);
    assert_eq!(shown(&chipped("abab", Some(weth))), none);

    // A status chip matches the tag a row shows: a blocked Shield received shows Blocked, and
    // an untagged received row counts as confirmed.
    let mut blocked = received_note();
    blocked.activity_classification = ActivityUtxoClassification::BlockedShield.label().to_owned();
    blocked.source_tx_hash =
        "0x4444444444444444444444444444444444444444444444444444444444444444".to_owned();
    let empty = ActivityData::default();
    let received = build_activity_rows(&ActivityRowsInput {
        data: &empty,
        utxos: &[received_note(), blocked.clone()],
        ..input
    })
    .rows;
    let status_shown = |status| -> Vec<ActivityRowId> {
        let mut filter = ActivityFilter::default();
        filter.chips = vec![ActivityChip::Status(status)];
        received
            .iter()
            .filter(|row| row_matches(row, &filter))
            .map(|row| row.id)
            .collect()
    };
    let received_id =
        |note: &UtxoOutput| ActivityRowId::Received(note.source_tx_hash.parse().unwrap());
    assert_eq!(
        status_shown(ActivityStatusChoice::Confirmed),
        [received_id(&received_note())]
    );
    assert_eq!(
        status_shown(ActivityStatusChoice::Blocked),
        [received_id(&blocked)]
    );

    // Suggestions, as (group, item titles).
    let assets = [
        (ActivityAsset::Token(usdc), "USDC".to_owned(), None),
        (ActivityAsset::Token(weth), "WETH".to_owned(), None),
    ];
    let suggest = |text: &str, pending: Option<ActivityFacet>| {
        activity_suggestions(&rows, &assets, chain, text, pending, &[])
    };
    let outline = |text: &str, pending: Option<ActivityFacet>| {
        suggest(text, pending)
            .into_iter()
            .map(|group| {
                let titles: Vec<String> = group.items.into_iter().map(|item| item.title).collect();
                (group.title, titles)
            })
            .collect::<Vec<_>>()
    };
    let titles =
        |titles: &[&str]| -> Vec<String> { titles.iter().map(|&title| title.to_owned()).collect() };
    // Free text offers itself first, then the filters, counterparties, tokens and operations
    // that hold it.
    assert_eq!(
        outline("h", None),
        [
            (None, titles(&["Search for h"])),
            (Some("Filters"), titles(&["Transaction"])),
            (Some("Counterparty"), titles(&["Alice Smith"])),
            (Some("Token"), titles(&["WETH"])),
            (Some("Operation"), titles(&["Shield"])),
        ]
    );
    // A pasted counterparty address offers its `with:` chip.
    let pasted = suggest(&friend.to_checksum(None), None);
    assert_eq!(pasted.len(), 2);
    assert_eq!(
        pasted[1].items[0].action,
        ActivitySuggestionAction::AddChip(ActivityChip::With(
            friend.to_checksum(None).to_lowercase()
        ))
    );
    // A typed value offers its `value:` chip.
    assert_eq!(
        suggest("> $12.5", None)[1].items[0].action,
        ActivitySuggestionAction::AddChip(ActivityChip::Value {
            above: true,
            usd_micro: U256::from(12_500_000_u64),
        })
    );
    // A bare amount under `value:` offers both directions.
    assert_eq!(
        outline("100", Some(ActivityFacet::Value)),
        [(Some("Value"), titles(&["Above $100", "Below $100"]))]
    );
    // A typed keyword picks its filter, whose values the rest of the text narrows.
    let (facet, rest) = ActivityFacet::split_keyword("Token: we").unwrap();
    assert_eq!(facet, ActivityFacet::Token);
    assert_eq!(
        outline(rest, Some(facet)),
        [(Some("Token"), titles(&["WETH"]))]
    );
    // A swap's recipient is a counterparty too.
    assert_eq!(
        outline("bob", Some(ActivityFacet::With)),
        [(Some("Counterparty"), titles(&["Bob"]))]
    );
}

#[gpui::test]
fn confirming_clear_history_clears_the_chosen_age_and_kinds(cx: &mut TestAppContext) {
    let stand_in = ActivityStandIn {
        data: ActivityData {
            entries: vec![entry(ActivityNetwork::Evm(1), ActivityKind::PrivateSend, 1)],
            ..ActivityData::default()
        },
        chain_id: 1,
        ..ActivityStandIn::default()
    };
    let path = temp_wallet_db_root("activity-clear");
    let runtime = test_runtime();
    let _entered = runtime.enter();
    let (root, cx) = activity_root(&path, &runtime, stand_in, cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.open_wallet_activity_clear_dialog(window, cx);
            let days = root.wallet_activity.clear_days_input().unwrap();
            days.update(cx, |input, cx| input.set_value("7", window, cx));
            for kind in ACTIVITY_KINDS
                .into_iter()
                .filter(|kind| !matches!(kind, ActivityKind::PrivateSend | ActivityKind::Unshield))
                .map(ActivityClearKind::Entry)
                .chain([ActivityClearKind::Received])
            {
                root.toggle_wallet_activity_clear_kind(kind, false, cx);
            }
            assert!(root.confirm_wallet_activity_clear(window, cx));
        });
    });
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.wallet_activity.clear_requests,
            vec![ActivityClear {
                age: ActivityClearAge::OlderThanDays(7),
                kinds: vec![ActivityKind::PrivateSend, ActivityKind::Unshield],
                received: false,
            }]
        );
    });

    // "All" with no kind chosen clears nothing and keeps the dialog open.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.set_wallet_activity_clear_older_than(false, cx);
            for kind in [ActivityKind::PrivateSend, ActivityKind::Unshield] {
                root.toggle_wallet_activity_clear_kind(ActivityClearKind::Entry(kind), false, cx);
            }
            assert!(!root.confirm_wallet_activity_clear(window, cx));
            root.toggle_wallet_activity_clear_kind(ActivityClearKind::Received, true, cx);
            assert!(root.confirm_wallet_activity_clear(window, cx));
        });
    });
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.wallet_activity.clear_requests.last(),
            Some(&ActivityClear {
                age: ActivityClearAge::All,
                kinds: Vec::new(),
                received: true,
            })
        );
    });
    close(root, cx, &path);
}

#[test]
fn activity_values_amounts_and_fees_at_todays_price_where_one_is_known() {
    use wallet_ops::vault::{ActivityAsset, ActivityDirection, ActivityFee, ActivityLeg};

    let chain = ActivityNetwork::Evm(1);
    let unpriced = alloy::primitives::Address::repeat_byte(0x77);
    let mut send = entry(chain, ActivityKind::PublicSend, 1_700_000_000);
    send.legs = vec![
        ActivityLeg {
            asset: ActivityAsset::Native,
            amount: U256::from(500_000_000_000_000_000_u64),
            direction: ActivityDirection::Out,
        },
        ActivityLeg {
            asset: ActivityAsset::Token(unpriced),
            amount: U256::from(1_000_u64),
            direction: ActivityDirection::Out,
        },
    ];
    send.fee = Some(ActivityFee {
        asset: ActivityAsset::Native,
        amount: U256::from(1_000_000_000_000_000_u64),
    });
    let data = ActivityData {
        entries: vec![send],
        ..ActivityData::default()
    };
    let prices = wallet_ops::TokenAnchorRateCache::new();
    // $3,000 per ETH, in USD micro.
    prices.store_native_usd_rate(1, U256::from(3_000_000_000_u64), 18);
    let running = BTreeSet::new();
    let input = |prices| ActivityRowsInput {
        network: chain,
        data: &data,
        utxos: &[],
        running: &running,
        labels: ActivityLabels {
            private: &[],
            public: &[],
            non_evm: &[],
            accounts: &[],
        },
        tokens: None,
        native: ("ETH", 18),
        prices,
    };

    let rows = build_activity_rows(&input(Some(&prices))).rows;
    let usd: Vec<_> = rows[0]
        .amounts
        .iter()
        .map(|amount| amount.usd.clone())
        .collect();
    assert_eq!(usd, [Some("$1,500.00".to_owned()), None]);
    let fee = rows[0].details.iter().find(|detail| detail.label == "Fee");
    assert_eq!(fee.unwrap().value, "0.001 ETH ($3.00)");
    // A `value:` chip compares the largest priced amount, $1,500.
    let valued = |above, usd: u64| {
        let mut filter = ActivityFilter::default();
        filter.chips = vec![ActivityChip::Value {
            above,
            usd_micro: U256::from(usd * 1_000_000),
        }];
        filter
    };
    assert!(row_matches(&rows[0], &valued(true, 1_000)));
    assert!(!row_matches(&rows[0], &valued(false, 1_000)));
    assert!(row_matches(&rows[0], &valued(false, 2_000)));

    // A network without pricing values nothing, which a `value:` chip hides.
    let rows = build_activity_rows(&input(None)).rows;
    assert!(rows[0].amounts.iter().all(|amount| amount.usd.is_none()));
    assert!(!row_matches(&rows[0], &valued(false, 2_000)));
}
