use super::*;
use wallet_ops::WalletConnectPendingRequest;

pub(super) const TEST_PASSWORD: &str = "correct horse battery staple";
pub(super) const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
pub(super) const TEST_IMPORTED_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945387e7d5e4a4dbd4b3f1b530b87d9b4a5c2f";

pub(super) fn test_kdf() -> KdfParams {
    KdfParams::new(1024, 1, 1)
}

pub(super) fn temp_db_root() -> PathBuf {
    let dir = std::env::temp_dir().join("railoxide-walletconnect-root-tests");
    fs::create_dir_all(&dir).expect("create temp db dir");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = WALLETCONNECT_RELAY_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("db-{pid}-{nanos}-{counter}"))
}

pub(super) fn walletconnect_test_store() -> (PathBuf, DesktopVaultStore) {
    let root_dir = temp_db_root();
    let store = DesktopVaultStore::open(root_dir.clone()).expect("open store");
    store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    (root_dir, store)
}

pub(super) fn import_test_wallet(
    store: &DesktopVaultStore,
    wallet_id: &str,
    label: &str,
) -> DesktopViewSession {
    let metadata = store
        .new_wallet_metadata(TEST_PASSWORD, wallet_id, 0, WalletSource::Imported, label)
        .expect("wallet metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            TEST_MNEMONIC,
            &metadata,
        )
        .expect("import wallet");
    store
        .load_view_session(TEST_PASSWORD, wallet_id)
        .expect("load wallet")
}

pub(super) fn test_walletconnect_session(topic: &str) -> WalletConnectSessionRecord {
    let mut approved_namespaces = BTreeMap::new();
    approved_namespaces.insert(
        "eip155".to_owned(),
        WalletConnectApprovedNamespace {
            chains: vec!["eip155:1".to_owned()],
            accounts: vec!["eip155:1:0x1111111111111111111111111111111111111111".to_owned()],
            methods: vec!["eth_sendTransaction".to_owned()],
            events: vec!["accountsChanged".to_owned()],
        },
    );
    WalletConnectSessionRecord {
        session_uuid: "session-uuid".to_owned(),
        pairing_topic: "pairing-topic".to_owned(),
        session_topic: topic.to_owned(),
        relay_protocol: "irn".to_owned(),
        relay_client_id: "relay-client".to_owned(),
        peer_metadata: WalletConnectPeerMetadata {
            name: "Aave".to_owned(),
            description: String::new(),
            url: "https://app.aave.com".to_owned(),
            icons: Vec::new(),
        },
        approved_namespaces,
        selected_public_account_uuid: "public-account".to_owned(),
        selected_public_account_scope: PublicAccountScope::Global,
        owning_private_wallet_uuid: None,
        keys: WalletConnectSessionKeys {
            sym_key: [1u8; 32],
            responder_private_key: [2u8; 32],
            responder_public_key: [3u8; 32],
        },
        expiry_timestamp: current_unix_seconds() + 300,
        lifecycle_state: WalletConnectSessionLifecycleState::Active,
    }
}

pub(super) fn test_walletconnect_relay_message(
    session: &WalletConnectSessionRecord,
    request: &WalletConnectJsonRpcRequest<Value>,
) -> WalletConnectRelayMessage {
    let plaintext = serde_json::to_vec(&request).expect("request json");
    let message = encode_walletconnect_message(&session.keys.sym_key, &plaintext)
        .expect("encode request")
        .to_base64();
    WalletConnectRelayMessage {
        topic: session.session_topic.clone(),
        message,
    }
}

pub(super) fn test_walletconnect_request(
    key: &str,
    expiry_timestamp: Option<u64>,
) -> WalletConnectRequestUi {
    let account = alloy::primitives::Address::from([0x11; 20]);
    WalletConnectRequestUi {
        request_control: None,
        rpc_reads: None,
        key: key.to_owned(),
        review_token: 1,
        binding: DappRequestBinding::from_walletconnect_session(&test_walletconnect_session(
            "session-topic",
        )),
        session_identity: DappSessionIdentity::walletconnect(
            test_walletconnect_session("session-topic").session_uuid,
        ),
        parsed: WalletConnectParsedRequest::EthAccounts,
        item: WalletConnectPendingRequest {
            id: 7,
            topic: "session-topic".to_owned(),
            dapp_name: "Aave".to_owned(),
            chain_id: "eip155:1".to_owned(),
            method: WalletConnectSupportedMethod::EthSendTransaction,
            account,
            decoded_transaction: None,
            raw_details: json!({}),
            expiry_timestamp,
        },
        account_source: PublicAccountSource::Imported,
    }
}
