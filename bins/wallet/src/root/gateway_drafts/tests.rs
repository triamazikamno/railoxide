use super::*;

fn synthetic_wallet() -> (
    std::path::PathBuf,
    DesktopVaultStore,
    Arc<DesktopViewSession>,
) {
    use wallet_ops::vault::{KdfParams, WalletSource};
    let path = std::env::temp_dir().join(format!("gateway-drafts-{:032x}", rand::random::<u128>()));
    let store = DesktopVaultStore::open(path.clone()).unwrap();
    let password = "synthetic draft test password";
    store
        .create_vault_with_params(password, KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = store
        .new_wallet_metadata(password, "wallet", 0, WalletSource::Imported, "Wallet")
        .unwrap();
    store.import_wallet_mnemonic_with_metadata(password, "wallet", 0, "english",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata).unwrap();
    let wallet = Arc::new(store.load_view_session(password, "wallet").unwrap());
    (path, store, wallet)
}

#[test]
fn selected_private_contact_is_reloaded_and_never_silently_retargeted() {
    use wallet_ops::gateway::{GatewayPrivateDraftInput, GatewayPrivateDraftKind};
    let (path, store, wallet) = synthetic_wallet();
    let entry = store
        .add_public_address_book_entry_for_session(
            &wallet,
            "Synthetic contact",
            "0x1111111111111111111111111111111111111111",
        )
        .unwrap();
    let mut input = GatewayPrivateDraftInput::default();
    input.kind = GatewayPrivateDraftKind::Unshield;
    input.address_book_entry = Some(entry.entry_uuid.clone());
    input.recipient = entry.address.to_checksum(None);
    assert_eq!(
        private::resolve_private_draft_contact(&store, &wallet, &input).unwrap(),
        input.recipient
    );
    let changed = store
        .update_public_address_book_entry_for_session(
            &wallet,
            &entry.entry_uuid,
            "Renamed contact",
            "0x2222222222222222222222222222222222222222",
        )
        .unwrap();
    assert!(private::resolve_private_draft_contact(&store, &wallet, &input).is_err());
    input.recipient = changed.address.to_checksum(None);
    assert_eq!(
        private::resolve_private_draft_contact(&store, &wallet, &input).unwrap(),
        input.recipient
    );
    input.address_book_entry = Some("removed-contact".into());
    assert!(private::resolve_private_draft_contact(&store, &wallet, &input).is_err());
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn private_drafts_share_bounded_peer_slots_and_reject_duplicate_or_stale_edits() {
    use wallet_ops::gateway::{GatewayPrivateDraftInput, GatewayPrivateDraftKind};
    let (path, store, wallet) = synthetic_wallet();
    let private_input = || {
        let mut input = GatewayPrivateDraftInput::default();
        input.wallet = "wallet".into();
        input.chain_id = 1;
        input.kind = GatewayPrivateDraftKind::PrivateSend;
        GatewayDraftPayload::Private(input)
    };
    let mut book = GatewayDraftBook::default();
    assert!(book.create("peer", "create".into(), private_input(), wallet.clone(), 1));
    let id = book.records["peer"].view.draft_id.clone();
    assert!(!book.create("peer", "create".into(), private_input(), wallet.clone(), 1));
    assert_eq!(book.records["peer"].view.draft_id, id);
    assert!(!book.update("other", &id, 1, private_input(), &wallet, 1));
    assert!(!book.update("peer", &id, 0, private_input(), &wallet, 1));
    assert!(!book.update("peer", &id, 1, private_input(), &wallet, 2));
    assert!(book.update("peer", &id, 2, private_input(), &wallet, 1));
    assert!(!book.update("peer", &id, 1, private_input(), &wallet, 1));
    let execution = GatewayDraftExecution::private("private-execution".into());
    assert!(execution.approve_review());
    assert!(execution.start());
    execution.bind_generation(1);
    book.records.get_mut("peer").unwrap().execution = Some(execution.clone());
    book.stopped(1); // The Public form may have the same generation counter.
    assert_eq!(execution.snapshot().status, GatewayDraftStatus::InProgress);
    assert!(!book.update("peer", &id, 3, private_input(), &wallet, 1));
    assert!(!book.create("peer", "another".into(), private_input(), wallet.clone(), 1));
    for ix in 1..MAX_DRAFT_PEERS {
        assert!(book.create(
            &format!("peer-{ix}"),
            "create".into(),
            private_input(),
            wallet.clone(),
            1
        ));
    }
    assert!(!book.create(
        "overflow",
        "create".into(),
        private_input(),
        wallet.clone(),
        1
    ));
    book.reconcile_wallet(Some(&wallet), 2);
    assert!(book.records.is_empty());
    drop(book);
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn each_draft_opens_its_own_client_and_a_new_account_replaces_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (path, store, wallet) = synthetic_wallet();
    let public_input = |account: &str| -> GatewayDraftPayload {
        GatewayDraftInput {
            account: account.to_owned(),
            chain_id: 1,
            kind: GatewayDraftKind::Send,
            asset: "native".to_owned(),
            amount: "0.1".to_owned(),
            recipient: String::new(),
            address_book_entry: None,
            fee: GatewayDraftFee::Normal,
            mimic_railway: false,
            approval: GatewayDraftApproval::default(),
            max: false,
        }
        .into()
    };
    let opens = Arc::new(AtomicUsize::new(0));
    let http = {
        let opens = Arc::clone(&opens);
        HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            wallet_ops::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            opens.fetch_add(1, Ordering::SeqCst);
            Ok(wallet_ops::OperationHttpClient::for_tests(
                reqwest::Client::new(),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ))
        })
    };
    let mut book = GatewayDraftBook::default();
    for peer in ["a", "b"] {
        assert!(book.create(
            peer,
            "create".into(),
            public_input("account-0"),
            wallet.clone(),
            1
        ));
    }
    // Two requests of draft a and one of draft b, all for the same account.
    for peer in ["a", "a", "b"] {
        let operation = book.records.get_mut(peer).unwrap().operation(&http);
        operation.client().await.unwrap();
    }
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    let id = book.records["a"].view.draft_id.clone();
    assert!(book.update("a", &id, 1, public_input("account-1"), &wallet, 1));
    let operation = book.records.get_mut("a").unwrap().operation(&http);
    operation.client().await.unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 3);
    drop(book);
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn dismiss_retires_editable_drafts_without_removing_live_executions() {
    use wallet_ops::gateway::GatewayPrivateDraftInput;
    let (path, store, wallet) = synthetic_wallet();
    let input = GatewayDraftPayload::Private(GatewayPrivateDraftInput::default());
    let mut book = GatewayDraftBook::default();
    assert!(book.create("peer", "first".into(), input.clone(), wallet.clone(), 1));
    let id = book.records["peer"].view.draft_id.clone();
    let estimate = tokio::spawn(std::future::pending::<()>());
    book.records.get_mut("peer").unwrap().estimation = Some(estimate.abort_handle());
    assert!(!book.dismiss("other-peer", &id));
    assert!(!book.dismiss("peer", "another-draft"));
    assert!(!estimate.is_finished());
    assert!(book.dismiss("peer", &id));
    assert!(estimate.await.unwrap_err().is_cancelled());
    assert!(!book.dismiss("peer", &id));
    assert!(book.create("peer", "next".into(), input, wallet.clone(), 1));
    let next_id = book.records["peer"].view.draft_id.clone();
    assert!(!book.dismiss("peer", &id));
    let execution = GatewayDraftExecution::private("execution".into());
    book.records.get_mut("peer").unwrap().execution = Some(execution.clone());
    // The editable view can lag behind the native approval/execution owner.
    assert!(!book.dismiss("peer", &next_id));
    assert!(execution.approve_review());
    assert!(execution.start());
    assert!(!book.dismiss("peer", &next_id));
    execution.stopped();
    assert!(book.dismiss("peer", &next_id));
    drop(book);
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[cfg(feature = "hardware")]
#[test]
fn hardware_refresh_carries_approved_public_executions_before_generation() {
    let (path, store, wallet) = synthetic_wallet();
    let refreshed = Arc::new(
        store
            .load_view_session("synthetic draft test password", "wallet")
            .unwrap(),
    );
    let input = GatewayDraftPayload::Public(GatewayDraftInput {
        account: "public".into(),
        chain_id: 1,
        kind: GatewayDraftKind::Send,
        asset: "native".into(),
        amount: "1".into(),
        recipient: String::new(),
        address_book_entry: None,
        fee: GatewayDraftFee::Normal,
        mimic_railway: false,
        approval: GatewayDraftApproval::Exact,
        max: false,
    });
    let mut book = GatewayDraftBook::default();
    for (peer, approved) in [("approved", true), ("unapproved", false)] {
        assert!(book.create(peer, "create".into(), input.clone(), wallet.clone(), 1));
        let execution = GatewayDraftExecution::default();
        if approved {
            assert!(execution.approve_review());
        }
        book.records.get_mut(peer).unwrap().execution = Some(execution);
    }
    book.refresh_hardware_session(&wallet, &refreshed, 1);
    book.reconcile_wallet(Some(&refreshed), 1);
    assert!(book.records.contains_key("approved"));
    assert!(!book.records.contains_key("unapproved"));
    drop(book);
    drop(refreshed);
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn private_gas_quotes_survive_form_edits_but_not_chain_or_delivery_changes() {
    use wallet_ops::gateway::{
        GatewayPrivateDelivery, GatewayPrivateDraftInput, GatewayPrivateFunding,
        GatewayPrivateGasFee, GatewayPrivateSelfBroadcastInput,
    };
    let (path, store, wallet) = synthetic_wallet();
    let mut input = GatewayPrivateDraftInput::default();
    input.wallet = "wallet".into();
    input.chain_id = 1;
    input.delivery = GatewayPrivateDelivery::SelfBroadcast {
        delivery: GatewayPrivateSelfBroadcastInput::SelfBroadcast {
            signer: None,
            funding: GatewayPrivateFunding::PublicBalance {},
            fee: GatewayPrivateGasFee::Auto {},
        },
    };
    let mut book = GatewayDraftBook::default();
    assert!(book.create(
        "peer",
        "create".into(),
        GatewayDraftPayload::Private(input.clone()),
        wallet.clone(),
        1,
    ));
    let id = book.records["peer"].view.draft_id.clone();
    book.records.get_mut("peer").unwrap().view.gas_quote =
        Some(GatewayDraftGasQuote::new("2".into(), "0.1".into()));
    input.amount = "1".into();
    assert!(book.update(
        "peer",
        &id,
        1,
        GatewayDraftPayload::Private(input.clone()),
        &wallet,
        1
    ));
    assert!(book.records["peer"].view.gas_quote.is_some());
    assert!(book.records["peer"].prepared.is_none());

    input.chain_id = 137;
    assert!(book.update(
        "peer",
        &id,
        2,
        GatewayDraftPayload::Private(input.clone()),
        &wallet,
        1
    ));
    assert!(book.records["peer"].view.gas_quote.is_none());

    book.records.get_mut("peer").unwrap().view.gas_quote =
        Some(GatewayDraftGasQuote::new("3".into(), "1".into()));
    input.delivery = GatewayPrivateDraftInput::default().delivery;
    assert!(book.update(
        "peer",
        &id,
        3,
        GatewayDraftPayload::Private(input),
        &wallet,
        1
    ));
    assert!(book.records["peer"].view.gas_quote.is_none());
    drop(book);
    drop(wallet);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn public_gas_quotes_follow_the_asset_fee_route() {
    let native = GatewayDraftInput {
        account: "account".into(),
        chain_id: 56,
        kind: GatewayDraftKind::Shield,
        asset: "native".into(),
        amount: "1".into(),
        recipient: String::new(),
        address_book_entry: None,
        fee: GatewayDraftFee::Normal,
        mimic_railway: true,
        approval: GatewayDraftApproval::Exact,
        max: false,
    };
    let token = GatewayDraftInput {
        asset: "0x55d398326f99059fF775485246999027B3197955".into(),
        ..native.clone()
    };
    let edited = GatewayDraftInput {
        amount: "2".into(),
        ..native.clone()
    };
    let [native, token, edited] = [native, token, edited].map(GatewayDraftPayload::Public);
    // On BNB the native Mimic quote is type 4 and the ERC-20 one legacy.
    assert!(quote_identity(&native) != quote_identity(&token));
    assert!(quote_identity(&native) == quote_identity(&edited));
}

#[test]
fn max_reserves_the_selected_fee_ceiling_and_token_max_still_requires_native_gas() {
    let input = GatewayDraftInput {
        account: "public".into(),
        chain_id: 1,
        kind: GatewayDraftKind::Send,
        asset: "native".into(),
        amount: String::new(),
        recipient: String::new(),
        address_book_entry: None,
        fee: GatewayDraftFee::Normal,
        mimic_railway: true,
        approval: GatewayDraftApproval::Exact,
        max: true,
    };
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 4,
    };
    let balance = U256::from(1_000_000_000_000_u64);
    let mut previous = U256::MAX;
    for fee in [
        GatewayDraftFee::Slow,
        GatewayDraftFee::Normal,
        GatewayDraftFee::Fast,
    ] {
        let fee = draft_gas_fee(
            &fee,
            quote,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            1,
        )
        .unwrap();
        let costs = estimate_public_action_gas_cost_with_profile_and_ceiling(
            1,
            &wallet_ops::settings::build_effective_chain_configs(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(1)
            .cloned()
            .unwrap(),
            PublicActionKind::Send,
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railoxide,
            fee,
            Some(quote),
            Some(fee),
        )
        .unwrap();
        let amount = draft_amount(
            &input,
            PublicAssetId::Native,
            18,
            balance,
            balance,
            costs.maximum_cost,
        )
        .unwrap();
        assert_eq!(amount + costs.maximum_cost, balance);
        assert!(amount < previous);
        previous = amount;
    }
    let token = PublicAssetId::Erc20(Address::repeat_byte(1));
    assert_eq!(
        draft_amount(&input, token, 6, balance, U256::from(7), U256::from(7)).unwrap(),
        balance
    );
    assert!(draft_amount(&input, token, 6, balance, U256::from(6), U256::from(7)).is_err());
    assert!(draft_amount(&input, PublicAssetId::Native, 18, balance, balance, balance).is_err());
    assert!(
        draft_gas_fee(
            &GatewayDraftFee::Custom {
                max_fee_gwei: "1".into(),
                priority_fee_gwei: "2".into()
            },
            quote,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            1
        )
        .is_err()
    );
}

#[test]
fn native_shield_drafts_bind_a_stealth_operation_only_when_prepared() {
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    let chain = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(1)
    .cloned()
    .unwrap();
    let key = |profile| NativeShieldKey {
        wallet_id: Some(Arc::from("wallet")),
        chain_id: 1,
        public_account_uuid: Arc::from("account"),
        profile,
        amount: U256::from(1_u64),
    };
    // As in estimation, a draft is prepared only on the stealth route.
    let bind = |bound: &mut _, payer, profile| {
        let prepared = matches!(
            draft_native_shield_route(1, &chain, payer, profile, None),
            Ok(NativeShieldRoute::Stealth(_))
        );
        bind_draft_native_operation(bound, prepared, key(profile)).unwrap()
    };
    let mut bound = None;
    for profile in [Railoxide, Railway] {
        assert!(bind(&mut bound, NativeShieldPayer::Software, profile).is_some());
    }
    // A fallback payer's draft has no preparation and leaves the bound operation alone.
    let software = bound.clone();
    assert_eq!(
        bind(&mut bound, NativeShieldPayer::Hardware, Railoxide),
        None
    );
    assert_eq!(bound, software);
}
