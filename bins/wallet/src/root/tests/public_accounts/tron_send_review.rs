use super::*;

/// Replaces the Tron account's balances in the latest refresh with `trx_sun` and any USDT.
fn set_tron_balances(root: &mut WalletRoot, trx_sun: u64, usdt: Option<U256>) {
    use wallet_ops::{PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry};
    let entry = |id, symbol: &str, amount| PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id,
            symbol: symbol.to_owned(),
            decimals: 6,
        },
        amount: PublicBalanceAmount::Available(amount),
    };
    let mut snapshot = root.tron_public.snapshot.as_deref().unwrap().clone();
    snapshot.accounts[0].balances =
        std::iter::once(entry(PublicAssetId::Native, "TRX", U256::from(trx_sun)))
            .chain(usdt.map(|amount| entry(PublicAssetId::Erc20(TRON_USDT), "USDT", amount)))
            .collect();
    root.tron_public.snapshot = Some(Arc::new(snapshot));
}

#[gpui::test]
fn a_late_tron_send_review_never_lands_in_another_form(cx: &mut TestAppContext) {
    use std::sync::atomic::Ordering::SeqCst;
    use wallet_ops::tron::transaction::TronAsset;
    let path = temp_wallet_db_root("tron-send-late-review");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    let mut intent = open_tron_send(&root, cx, &runtime, &chain);
    let uuid = Arc::clone(&intent.account_uuid);
    let review = |cx: &mut gpui::VisualTestContext| {
        root.read_with(cx, |root, _| {
            root.tron_public
                .send
                .form
                .as_ref()
                .and_then(|form| form.review)
        })
    };
    let reviews_usdt = |cx: &mut gpui::VisualTestContext| {
        matches!(
            review(cx).map(|review| review.transfer.asset),
            Some(TronAsset::Trc20 { contract, .. }) if contract == TRON_USDT
        )
    };

    // The TRX form's preparation is still reading when the user opens USDT's instead and makes
    // the same edits.
    chain.hold_next_preparation.store(true, SeqCst);
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 10 * TRX_SUN, Some(U256::from(5 * TRX_SUN)));
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        set_tron_test_receiver(root, window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |_| !chain.hold_next_preparation.load(SeqCst));
    root.update_in(cx, |root, window, cx| {
        root.close_tron_send_form(window, cx);
        root.open_tron_send_dialog(
            Arc::clone(&uuid),
            PublicAssetId::Erc20(TRON_USDT),
            window,
            cx,
        );
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        set_tron_test_receiver(root, window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    assert!(reviews_usdt(cx));
    chain.release_preparation.store(true, SeqCst);
    runtime.block_on(tokio::time::sleep(Duration::from_millis(100)));
    cx.run_until_parked();
    assert!(reviews_usdt(cx), "the TRX form's review landed in USDT's");

    // Nor does a new review of an approved TRX send whose fees rose.
    intent.review.fees.maximum -= 1;
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |cx| {
        !root.read_with(cx, |root, _| root.public_form.sending)
    });
    assert!(
        reviews_usdt(cx),
        "the TRX send's new review landed in USDT's form"
    );
    root.read_with(cx, |root, _| {
        assert_eq!(root.tron_public.send.form.as_ref().unwrap().error, None);
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_token_send_notes_a_receiver_that_does_not_hold_the_token(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-send-receiver-note");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    let uuid = open_tron_send(&root, cx, &runtime, &chain).account_uuid;
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 100 * TRX_SUN, Some(U256::from(5 * TRX_SUN)));
        root.close_tron_send_form(window, cx);
        root.open_tron_send_dialog(uuid, PublicAssetId::Erc20(TRON_USDT), window, cx);
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
    });

    // The test receiver's transfer costs what the transfer back to the sender does.
    for (receiver, noted) in [
        (alloy::primitives::Address::repeat_byte(0x22), false),
        (TRON_TOKENLESS_RECEIVER, true),
    ] {
        root.update_in(cx, |root, window, cx| {
            let receiver = wallet_ops::tron::address::encode_tron_address(receiver);
            root.set_tron_receiver(&receiver, window, cx);
        });
        start_tron_send_preparation(cx);
        drive_until(cx, &runtime, |cx| {
            root.read_with(cx, |root, _| {
                root.tron_public
                    .send
                    .form
                    .as_ref()
                    .is_some_and(|form| form.review.is_some())
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            cx.debug_bounds("wallet-tron-send-receiver-note").is_some(),
            noted,
            "{receiver}"
        );
    }

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_send_reviews_the_latest_trx_balance(cx: &mut TestAppContext) {
    use wallet_ops::tron::transaction::TronAsset;
    let path = temp_wallet_db_root("tron-send-latest-balance");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    // The form opens with 10 TRX.
    open_tron_send(&root, cx, &runtime, &chain);
    let review = |cx: &mut gpui::VisualTestContext| {
        root.read_with(cx, |root, _| {
            root.tron_public
                .send
                .form
                .as_ref()
                .and_then(|form| form.review)
        })
    };

    // Max after a refresh lowered the balance spends the new balance. The receiver is the
    // picker's second suggestion: open it, move down, press Enter.
    root.update_in(cx, |root, window, cx| {
        use ui::recipient_picker::{RecipientPickerEvent, RecipientSuggestionDirection};
        use wallet_ops::tron::address::encode_tron_address;
        set_tron_balances(root, 4 * TRX_SUN, None);
        let tron = |byte| encode_tron_address(alloy::primitives::Address::repeat_byte(byte));
        root.non_evm_address_book = [0x33, 0x22]
            .map(|byte| wallet_ops::vault::NonEvmAddressBookEntry {
                entry_uuid: format!("entry-{byte}"),
                label: format!("Entry {byte}"),
                network: wallet_ops::bridge::NonEvmNetwork::Tron,
                address: tron(byte),
                display_order: 0,
            })
            .into();
        root.tron_receiver_picker_event(&RecipientPickerEvent::Toggle, window, cx);
        let next = RecipientPickerEvent::Move(RecipientSuggestionDirection::Next);
        root.tron_receiver_picker_event(&next, window, cx);
        root.confirm_tron_receiver_suggestion(window, cx);
        let form = root.tron_public.send.form.as_ref().unwrap();
        assert_eq!(form.receiver_value, tron(0x22));
        assert!(!form.receiver_picker.open());
        root.set_tron_send_amount_to_max(window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    let max = review(cx).unwrap();
    assert_eq!(max.trx_balance, 4 * TRX_SUN);
    let TronAsset::Trx { amount_sun } = max.transfer.asset else {
        panic!("a TRX send");
    };
    assert!(amount_sun <= 4 * TRX_SUN - max.fees.maximum);

    // A review from before a refresh is prepared again instead of approved.
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 3 * TRX_SUN, None);
        root.submit_tron_send_from_form(window, cx);
        let form = root.tron_public.send.form.as_ref().unwrap();
        assert!(form.review.is_none() && form.preparing);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    assert_eq!(review(cx).unwrap().trx_balance, 3 * TRX_SUN);

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
