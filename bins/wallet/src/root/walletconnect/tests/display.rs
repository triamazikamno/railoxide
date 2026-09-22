use super::fixtures::*;
use super::*;

#[test]
fn request_dialog_nav_reports_position_and_edges() {
    let mut pending_requests = BTreeMap::new();
    pending_requests.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", None),
    );
    pending_requests.insert(
        "session-topic:2".to_owned(),
        test_walletconnect_request("session-topic:2", None),
    );
    pending_requests.insert(
        "session-topic:3".to_owned(),
        test_walletconnect_request("session-topic:3", None),
    );

    assert_eq!(
        walletconnect_request_dialog_nav(&pending_requests, "session-topic:1"),
        Some(WalletConnectRequestDialogNav {
            index: 1,
            total: 3,
            previous_key: None,
            next_key: Some("session-topic:2".to_owned()),
        })
    );
    assert_eq!(
        walletconnect_request_dialog_nav(&pending_requests, "session-topic:2"),
        Some(WalletConnectRequestDialogNav {
            index: 2,
            total: 3,
            previous_key: Some("session-topic:1".to_owned()),
            next_key: Some("session-topic:3".to_owned()),
        })
    );
    assert_eq!(
        walletconnect_request_dialog_nav(&pending_requests, "session-topic:3"),
        Some(WalletConnectRequestDialogNav {
            index: 3,
            total: 3,
            previous_key: Some("session-topic:2".to_owned()),
            next_key: None,
        })
    );
}

#[test]
fn request_dialog_nav_returns_none_for_missing_request() {
    let mut pending_requests = BTreeMap::new();
    pending_requests.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", None),
    );

    assert_eq!(
        walletconnect_request_dialog_nav(&pending_requests, "session-topic:missing"),
        None
    );
}

#[test]
fn approved_chain_display_items_use_known_chain_names_and_icons() {
    let mut session = test_walletconnect_session("chain-display-topic");
    session
        .approved_namespaces
        .get_mut("eip155")
        .expect("eip155 namespace")
        .chains = vec![
        "eip155:42161".to_owned(),
        "eip155:1".to_owned(),
        "eip155:42161".to_owned(),
        "eip155:56".to_owned(),
    ];

    let items = approved_chain_display_items(&session);
    let labels = items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(labels, vec!["Arbitrum", "BSC", "Ethereum"]);
    assert!(items.iter().all(|item| item.icon_path.is_some()));
    assert!(labels.iter().all(|label| !label.contains("eip155")));
}

#[test]
fn approved_chain_display_items_fall_back_to_raw_unknown_chain() {
    let mut session = test_walletconnect_session("unknown-chain-display-topic");
    session
        .approved_namespaces
        .get_mut("eip155")
        .expect("eip155 namespace")
        .chains = vec!["eip155:31337".to_owned()];

    let items = approved_chain_display_items(&session);

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label, "eip155:31337");
    assert_eq!(items[0].icon_path, None);
}

#[test]
fn format_unix_seconds_uses_local_datetime() {
    let formatted = format_unix_seconds(1_700_000_000);

    assert_ne!(formatted, "1700000000");
    assert!(formatted.contains('-'));
    assert!(formatted.contains(':'));
}

#[test]
fn request_expiry_status_is_missing_bounded_and_expired() {
    assert_eq!(
        walletconnect_request_expiry_status(None, 100),
        WalletConnectRequestExpiryStatus::Missing
    );
    assert_eq!(
        walletconnect_request_expiry_status(Some(125), 100),
        WalletConnectRequestExpiryStatus::Remaining(25)
    );
    assert_eq!(
        walletconnect_request_expiry_status(Some(100), 101),
        WalletConnectRequestExpiryStatus::Expired
    );
    assert_eq!(
        walletconnect_request_expiry_status(Some(0), u64::MAX),
        WalletConnectRequestExpiryStatus::Expired
    );
    assert!(walletconnect_request_approval_admitted(None, u64::MAX));
    assert!(!walletconnect_request_approval_admitted(Some(100), 100));
}

#[test]
fn request_disclosures_are_isolated_and_pruned_when_requests_disappear() {
    let mut states = BTreeMap::from([
        (
            "session-topic:1".to_owned(),
            WalletConnectRequestDisclosureState::default(),
        ),
        (
            "session-topic:2".to_owned(),
            WalletConnectRequestDisclosureState::default(),
        ),
    ]);
    toggle_request_disclosure_state(
        states.get_mut("session-topic:1").expect("first state"),
        WalletConnectRequestDisclosure::TransactionDetails,
    );
    toggle_request_disclosure_state(
        states.get_mut("session-topic:1").expect("first state"),
        WalletConnectRequestDisclosure::NetworkFee,
    );
    toggle_request_disclosure_state(
        states.get_mut("session-topic:2").expect("second state"),
        WalletConnectRequestDisclosure::RawRequest,
    );

    assert!(states["session-topic:1"].transaction_details_open);
    assert!(states["session-topic:1"].network_fee_open);
    assert!(!states["session-topic:1"].raw_request_open);
    assert!(!states["session-topic:2"].transaction_details_open);
    assert!(!states["session-topic:2"].network_fee_open);
    assert!(states["session-topic:2"].raw_request_open);

    let replacement = states.insert(
        "session-topic:1".to_owned(),
        WalletConnectRequestDisclosureState::default(),
    );
    assert!(replacement.is_some());
    assert!(!states["session-topic:1"].network_fee_open);
    assert!(!states["session-topic:1"].transaction_details_open);
    assert!(!states["session-topic:1"].raw_request_open);

    let mut pending = BTreeMap::new();
    pending.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", None),
    );
    prune_request_disclosure_states(&mut states, &pending);
    assert_eq!(states.len(), 1);
    assert!(states.contains_key("session-topic:1"));
}
