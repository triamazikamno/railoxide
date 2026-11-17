use super::*;
use alloy::primitives::Address;

#[test]
fn used_filter_includes_a_consumed_nonce_without_a_restore_check() {
    use alloy::{
        eips::BlockNumHash,
        primitives::{B256, Bytes, U256},
    };
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload,
    };

    let block = BlockNumHash::new(10, B256::repeat_byte(10));
    let hash = B256::repeat_byte(1);
    let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
        U256::ZERO,
        Address::ZERO,
        hash,
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(block, U256::ZERO),
            Vec::new(),
        ),
    ))
    .unwrap();
    payload["transaction_hashes"] = serde_json::json!([hash]);
    // The account's nonce is recorded as unconsumed, then as consumed. No Restore
    // observation is present.
    let records = [0_u64, 1]
        .into_iter()
        .map(|consumed| {
            serde_json::from_value::<ExecutorRecord>(serde_json::json!({
                "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
                "operation": ExecutorOperationId::random().unwrap(), "index": consumed,
                "address": Address::ZERO, "delegate": Address::ZERO,
                "retired": true, "issued": [payload.clone()],
                "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), 10),
            }))
            .unwrap()
        })
        .collect::<Vec<_>>();
    let used = visible_accounts(
        &records,
        AccountFilter::Used,
        false,
        "",
        |_| AccountVisibility {
            attention: false,
            unresolved: false,
            holding: false,
        },
        |_| String::new(),
    );
    assert_eq!(
        used,
        vec![records[1].operation()],
        "a consumed nonce must count while an unconfirmed transaction must not"
    );
}

#[test]
fn large_collection_keeps_hidden_issues_findable_and_orders_known_times_before_index_fallback() {
    let records = (0..1_000_u32)
        .map(|index| {
            serde_json::from_value::<ExecutorRecord>(serde_json::json!({
                "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
                "operation": ExecutorOperationId::random().unwrap(), "index": index,
                "address": Address::ZERO, "delegate": Address::ZERO,
                "retired": true, "hidden": index % 2 == 0,
                "created_at": if index == 2 { Some(100) } else { None },
                "restored_at": if index == 4 { Some(200) } else { None },
                "purpose_summary": if index == 2 { "Unshield WETH" } else { "Earlier operation" },
                "assets": [ExecutorAsset::Native], "issued": [],
                "use_check": {
                    "observation": (index <= 3 && index != 0).then(|| serde_json::json!({
                        "block": alloy::eips::BlockNumHash::new(10, alloy::primitives::B256::ZERO),
                        "checked_at": 200,
                        "nonce": (index != 3).then_some(alloy::primitives::U256::ZERO),
                    })),
                    "unavailable": index == 2,
                },
            }))
            .unwrap()
        })
        .collect::<Vec<_>>();
    let visibility = |record: &ExecutorRecord| AccountVisibility {
        attention: record.index() == 2,
        unresolved: record.index() == 6,
        holding: record.index() == 2,
    };
    let used = visible_accounts(&records, AccountFilter::Used, false, "", visibility, |_| {
        String::new()
    });
    assert_eq!(
        used,
        vec![records[2].operation(), records[1].operation()],
        "Used includes decoded zero and a retained success after failure, excluding empty or absent evidence"
    );
    let all = visible_accounts(&records, AccountFilter::All, false, "", visibility, |_| {
        String::new()
    });
    assert_eq!(all.len(), 502);
    assert_eq!(all[0], records[2].operation());
    assert_eq!(all[1], records[999].operation());
    assert!(all.contains(&records[6].operation()));
    assert!(!all.contains(&records[4].operation()));
    let attention = visible_accounts(
        &records,
        AccountFilter::Attention,
        false,
        "weth",
        visibility,
        |_| String::new(),
    );
    assert_eq!(attention, vec![records[2].operation()]);
    assert_eq!(
        visible_accounts(
            &records,
            AccountFilter::Holding,
            false,
            "",
            visibility,
            |_| String::new()
        ),
        attention
    );
    let shown = visible_accounts(&records, AccountFilter::All, true, "", visibility, |_| {
        String::new()
    });
    assert_eq!(
        &shown[..2],
        &[records[4].operation(), records[2].operation()]
    );
    // An account-local token label is searchable even without a purpose match.
    let token = visible_accounts(
        &records,
        AccountFilter::All,
        true,
        "dai",
        visibility,
        |record| {
            if record.index() == 4 {
                "DAI".into()
            } else {
                String::new()
            }
        },
    );
    assert_eq!(token, vec![records[4].operation()]);
}

struct PagedAccountsHost {
    pages: AccountPages,
    total: usize,
    target: ExecutorOperationId,
    picked: std::rc::Rc<std::cell::Cell<Option<ExecutorOperationId>>>,
}

impl Render for PagedAccountsHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let range = self.pages.range();
        let rows = self.pages.accounts[range.clone()]
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let operation = *id;
                let picked = self.picked.clone();
                account_rows(
                    TableRow::new().child(
                        TableCell::new().col_span(5).child(
                            app_button(SharedString::from(operation.opaque_id()), "Open account")
                                .when(index + 1 == range.len(), |button| {
                                    button.debug_selector(|| "page-last-account".into())
                                })
                                .when(operation == self.target, |button| {
                                    button.debug_selector(|| "page-target-account".into())
                                })
                                .on_click(move |_, _, _| picked.set(Some(operation))),
                        ),
                    ),
                    (index == 0).then(|| div().h(rems(12.)).child("Expanded inspector")),
                    cx,
                )
            })
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(account_table(
                &self.pages,
                TableHeader::new()
                    .child(TableRow::new().child(TableHead::new().col_span(5).child("Accounts"))),
                rows,
            ))
            .child(page_footer(
                &self.pages,
                self.total,
                0,
                cx.listener(|this, page, _, cx| {
                    this.pages.change(*page - 1);
                    cx.notify();
                }),
            ))
    }
}

#[gpui::test]
fn paging_and_scrolling_keep_every_account_reachable(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use gpui_component::Root;
    cx.update(gpui_component::init);
    cx.update(ui::theme::apply_zenburn_component_theme);
    let accounts = (0..64)
        .map(|_| ExecutorOperationId::random().unwrap())
        .collect::<Vec<_>>();
    let picked = std::rc::Rc::new(std::cell::Cell::new(None));
    let (host, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| {
            let mut pages = AccountPages::default();
            pages.update(accounts.clone());
            PagedAccountsHost {
                pages,
                total: accounts.len(),
                target: accounts[35],
                picked: picked.clone(),
            }
        });
        Root::new(view, window, cx)
    });
    let host = host.read_with(cx, |root, _| {
        root.view().clone().downcast::<PagedAccountsHost>().unwrap()
    });
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(600.)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for (page, last) in [(0, 24), (1, 49), (2, 63)] {
        if page != 0 {
            let controls = cx
                .debug_bounds("stealth-pagination")
                .expect("Page controls");
            cx.simulate_click(
                gpui::point(controls.right() - gpui::px(20.), controls.center().y),
                gpui::Modifiers::none(),
            );
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert_eq!(host.read_with(cx, |host, _| host.pages.page), page);
        cx.simulate_event(gpui::ScrollEvent {
            position: gpui::point(gpui::px(100.), gpui::px(250.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-5000.))),
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let last_row = cx
            .debug_bounds("page-last-account")
            .expect("Last account on the page");
        cx.simulate_click(last_row.center(), gpui::Modifiers::none());
        assert_eq!(
            picked.get(),
            Some(accounts[last]),
            "The page must scroll to its last account"
        );
    }
    // A recovery link also works in a narrow window with enlarged text.
    cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(420.)));
    cx.update(|window, cx| {
        window.set_rem_size(gpui::px(20.));
        host.update(cx, |host, cx| {
            host.pages.reveal(accounts[35]);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    let target = cx
        .debug_bounds("page-target-account")
        .expect("Recovery target");
    cx.simulate_click(target.center(), gpui::Modifiers::none());
    assert_eq!(picked.get(), Some(accounts[35]));
    // A narrower result set cannot leave an empty, out-of-range page.
    cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.pages.update(vec![accounts[60]]);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    let only = cx
        .debug_bounds("page-last-account")
        .expect("Filtered account");
    cx.simulate_click(only.center(), gpui::Modifiers::none());
    assert_eq!(picked.get(), Some(accounts[60]));
    assert_eq!(host.read_with(cx, |host, _| host.pages.page), 0);
}
