use wallet_ops::bridge::NonEvmNetwork;
use wallet_ops::vault::NonEvmAddressBookEntry;

use super::helpers::temp_wallet_db_root;
use crate::root::address_book::{
    AddressBookDetectedType, AddressBookEntryKind, address_book_detected_type,
    address_book_entry_matches_search, address_book_entry_validation_message,
    filtered_non_evm_address_book_entries,
};

const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";
const PUBLIC_ADDRESS: &str = "0x1111111111111111111111111111111111111111";
const SOLANA_ADDRESS: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const BITCOIN_ADDRESS: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
/// A unified address with Orchard and P2PKH receivers.
const ZCASH_UNIFIED_ADDRESS: &str = "u17lpmx8t5cyk0uaaw0tzmsa8zs23w4wj6uteg4qhlqutpgeqeumtwjaa06l2x8af8gqkkw05fylg622czjahnwl6vtyqasrtzxawwtf0a2zak64zk7f52v2gsh7rqj6ln63sjz38v09h";
const ZCASH_SAPLING_ADDRESS: &str =
    "zs1z7rejlpsa98s2rrrfkwmaxu53e4ue0ulcrw0h4x5g8jl04tak0d3mm47vdtahatqrlkngh9slya";

#[test]
fn address_book_type_detection_uses_recipient_prefixes() {
    assert_eq!(
        address_book_detected_type(RAILGUN_ADDRESS),
        AddressBookDetectedType::Private
    );
    assert_eq!(
        address_book_detected_type(PUBLIC_ADDRESS),
        AddressBookDetectedType::Public
    );
    assert_eq!(
        address_book_detected_type("not-an-address"),
        AddressBookDetectedType::Unknown
    );
}

#[test]
fn address_book_validation_rejects_invalid_and_type_switched_values() {
    assert_eq!(
        address_book_entry_validation_message(None, "Alice", RAILGUN_ADDRESS),
        None
    );
    assert_eq!(
        address_book_entry_validation_message(None, "Bob", PUBLIC_ADDRESS),
        None
    );
    assert_eq!(
        address_book_entry_validation_message(
            None,
            "Bob",
            "0xd8DA6BF26964aF9D7eEd9e03E53415D37aA96045"
        ),
        Some("Enter a valid public EVM address")
    );
    assert_eq!(
        address_book_entry_validation_message(None, "  ", PUBLIC_ADDRESS),
        Some("Enter a label")
    );
    assert_eq!(
        address_book_entry_validation_message(None, "Bob", "not-an-address"),
        Some(
            "Enter a 0zk private recipient, 0x public EVM address, or Bitcoin, Solana, Tron or Zcash address"
        )
    );
    assert_eq!(
        address_book_entry_validation_message(None, "Shielded", ZCASH_SAPLING_ADDRESS),
        Some(
            "Sapling-only addresses aren't supported. Enter a t1… address, or a u1… address with \
             an Orchard or transparent receiver"
        )
    );
    assert_eq!(
        address_book_entry_validation_message(None, "Alice", "0zk-invalid"),
        Some("Enter a valid private 0zk recipient")
    );
    assert_eq!(
        address_book_entry_validation_message(
            Some(AddressBookEntryKind::Private),
            "Alice",
            PUBLIC_ADDRESS,
        ),
        Some("Private address-book entries must use a 0zk recipient")
    );
    assert_eq!(
        address_book_entry_validation_message(
            Some(AddressBookEntryKind::Public),
            "Bob",
            RAILGUN_ADDRESS,
        ),
        Some("Public address-book entries must use a 0x EVM address")
    );
    // Editing keeps a non-EVM entry on its network.
    assert_eq!(
        address_book_entry_validation_message(
            Some(AddressBookEntryKind::NonEvm(NonEvmNetwork::Bitcoin)),
            "Cold",
            SOLANA_ADDRESS,
        ),
        Some("Bitcoin address-book entries must use a Bitcoin address")
    );
}

#[gpui::test]
fn address_book_adds_non_evm_addresses_from_the_single_flow(cx: &mut gpui::TestAppContext) {
    let path = temp_wallet_db_root("address-book-non-evm");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = super::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update_in(cx, |root, window, cx| {
        root.address_book
            .add_label_input
            .update(cx, |input, cx| input.set_value("Phantom", window, cx));
        root.address_book
            .add_address_input
            .update(cx, |input, cx| input.set_value(SOLANA_ADDRESS, window, cx));
        assert!(root.add_address_book_entry_from_dialog(window, cx));
        assert!(root.private_address_book.is_empty() && root.public_address_book.is_empty());
        let [entry] = root.non_evm_address_book.as_slice() else {
            panic!("one non-EVM entry: {:?}", root.non_evm_address_book);
        };
        assert_eq!(
            (entry.label.as_str(), entry.network, entry.address.as_str()),
            ("Phantom", NonEvmNetwork::Solana, SOLANA_ADDRESS)
        );

        root.address_book
            .add_label_input
            .update(cx, |input, cx| input.set_value("Zashi", window, cx));
        root.address_book.add_address_input.update(cx, |input, cx| {
            input.set_value(ZCASH_UNIFIED_ADDRESS, window, cx);
        });
        assert!(root.add_address_book_entry_from_dialog(window, cx));
        assert_eq!(
            root.non_evm_address_book
                .iter()
                .map(|entry| (entry.label.as_str(), entry.network, entry.address.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Phantom", NonEvmNetwork::Solana, SOLANA_ADDRESS),
                ("Zashi", NonEvmNetwork::Zcash, ZCASH_UNIFIED_ADDRESS),
            ]
        );
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn address_book_search_matches_label_and_full_address_case_insensitively() {
    assert!(address_book_entry_matches_search(
        "Alice Savings",
        PUBLIC_ADDRESS,
        "savings",
    ));
    assert!(address_book_entry_matches_search(
        "Alice Savings",
        PUBLIC_ADDRESS,
        "1111111111",
    ));
    assert!(address_book_entry_matches_search(
        "Mixed Case",
        PUBLIC_ADDRESS,
        "mixed case",
    ));
    assert!(!address_book_entry_matches_search(
        "Alice Savings",
        PUBLIC_ADDRESS,
        "charlie",
    ));

    let non_evm = [
        (NonEvmNetwork::Bitcoin, BITCOIN_ADDRESS),
        (NonEvmNetwork::Solana, SOLANA_ADDRESS),
    ]
    .map(|(network, address)| NonEvmAddressBookEntry {
        entry_uuid: address.to_owned(),
        label: "Exchange".to_owned(),
        network,
        address: address.to_owned(),
        display_order: 0,
    });
    let matches = filtered_non_evm_address_book_entries(&non_evm, "wDD5aUFQ");
    assert_eq!(
        matches
            .iter()
            .map(|entry| entry.address.as_str())
            .collect::<Vec<_>>(),
        [SOLANA_ADDRESS]
    );
}
