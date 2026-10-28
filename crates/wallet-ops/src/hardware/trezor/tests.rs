use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use alloy::primitives::U256;
use serde_json::json;
use trezor_client::TrezorMessage;
use trezor_client::protos::MessageType;
use trezor_client::transport::{ProtoMessage, Transport, error::Error as TrezorTransportError};
use zeroize::Zeroizing;

use super::super::{
    HardwareAppVersion, HardwareDerivationDescriptor, HardwareDerivationError, HardwareDeviceKind,
    HardwarePublicAccountDescriptor, HardwareTypedDataSigningMode, HardwareWalletSyncIntent,
    TEST_ACCOUNT_CHAIN_CODE, TEST_ACCOUNT_PUBLIC_KEY,
};
use super::client::{
    TrezorDeviceInfo, TrezorHardwareDerivationClient, trezor_account_public_key_from_node,
    trezor_ethereum_get_address_request, trezor_ethereum_get_public_key_request,
};
use super::passphrase::{TrezorPassphraseState, TrezorPinMatrixRequestKind};
use super::transaction::{TREZOR_ETHEREUM_TX_CHUNK_SIZE, TrezorLegacySignRequest};
use super::typed_data::{
    TrezorEthereumDataType, classify_trezor_typed_data_signing_mode, trezor_typed_data_struct_ack,
    trezor_typed_data_value,
};
use crate::hardware_typed_data::HardwareEip712Model;
use crate::vault::TrezorPassphraseMode;

type RecordedMessages = Arc<Mutex<Vec<(MessageType, Vec<u8>)>>>;

struct QueuedTransport {
    responses: VecDeque<ProtoMessage>,
    writes: Arc<Mutex<Vec<MessageType>>>,
}

struct RecordingTransport {
    responses: VecDeque<ProtoMessage>,
    writes: RecordedMessages,
}

impl Transport for QueuedTransport {
    fn session_begin(&mut self) -> Result<(), TrezorTransportError> {
        Ok(())
    }

    fn session_end(&mut self) -> Result<(), TrezorTransportError> {
        Ok(())
    }

    fn write_message(&mut self, message: ProtoMessage) -> Result<(), TrezorTransportError> {
        self.writes
            .lock()
            .expect("writes lock")
            .push(message.message_type());
        Ok(())
    }

    fn read_message(&mut self) -> Result<ProtoMessage, TrezorTransportError> {
        self.responses
            .pop_front()
            .ok_or_else(|| TrezorTransportError::IO(std::io::Error::other("no queued response")))
    }
}

impl Transport for RecordingTransport {
    fn session_begin(&mut self) -> Result<(), TrezorTransportError> {
        Ok(())
    }

    fn session_end(&mut self) -> Result<(), TrezorTransportError> {
        Ok(())
    }

    fn write_message(&mut self, message: ProtoMessage) -> Result<(), TrezorTransportError> {
        let message_type = message.message_type();
        self.writes
            .lock()
            .expect("writes lock")
            .push((message_type, message.into_payload()));
        Ok(())
    }

    fn read_message(&mut self) -> Result<ProtoMessage, TrezorTransportError> {
        self.responses
            .pop_front()
            .ok_or_else(|| TrezorTransportError::IO(std::io::Error::other("no queued response")))
    }
}

#[derive(Clone)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("logs lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `run` under a TRACE subscriber and returns what it logged. A `trezor_client` trace
/// event is emitted first and checked, so the capture can't pass by filtering everything out.
fn capture_trace_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogCapture(Arc::clone(&logs));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let value = tracing::subscriber::with_default(subscriber, || {
        tracing::trace!(target: "trezor_client", "capture check");
        run()
    });
    let logs = String::from_utf8(logs.lock().expect("logs lock").clone()).expect("UTF-8 logs");
    assert!(logs.contains("capture check"));
    (value, logs)
}

fn queued_message<M: TrezorMessage>(message: &M) -> ProtoMessage {
    ProtoMessage(
        M::MESSAGE_TYPE,
        message.write_to_bytes().expect("encode test message"),
    )
}

fn recording_client(
    responses: VecDeque<ProtoMessage>,
    writes: RecordedMessages,
) -> TrezorHardwareDerivationClient {
    let client = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(RecordingTransport { responses, writes }),
    );
    TrezorHardwareDerivationClient {
        client,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    }
}

fn typed_data_model() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Person": [
                { "name": "wallet", "type": "address" },
                { "name": "name", "type": "string" }
            ],
            "Message": [
                { "name": "count", "type": "uint256" },
                { "name": "person", "type": "Person" },
                { "name": "tags", "type": "string[2]" },
                { "name": "payload", "type": "bytes" },
                { "name": "digest", "type": "bytes32" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "count": 128,
            "person": {
                "wallet": "0x1111111111111111111111111111111111111111",
                "name": "Alice"
            },
            "tags": ["permit", "test"],
            "payload": "0x010203",
            "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    }))
    .expect("typed-data model")
}

fn reordered_domain_typed_data_model() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(json!({
        "types": {
            "EIP712Domain": [
                { "name": "chainId", "type": "uint256" },
                { "name": "name", "type": "string" }
            ],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "contents": "hello"
        }
    }))
    .expect("typed-data model")
}

fn trezor_descriptor() -> HardwarePublicAccountDescriptor {
    HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Trezor, 0, 0)
        .expect("trezor descriptor")
}

fn typed_data_signature() -> trezor_client::protos::EthereumTypedDataSignature {
    let mut message = trezor_client::protos::EthereumTypedDataSignature::new();
    let mut signature = Vec::new();
    signature.extend_from_slice(&[1; 32]);
    signature.extend_from_slice(&[2; 32]);
    signature.push(27);
    message.set_signature(signature);
    message.set_address("0x1111111111111111111111111111111111111111".to_owned());
    message
}

fn test_features(session_id: Option<Vec<u8>>) -> trezor_client::protos::Features {
    let mut features = trezor_client::protos::Features::new();
    features.set_vendor("trezor.io".to_owned());
    features.set_major_version(2);
    features.set_minor_version(8);
    features.set_patch_version(0);
    features.set_initialized(true);
    if let Some(session_id) = session_id {
        features.set_session_id(session_id);
    }
    features
}

#[test]
fn ethereum_signing_flow_handles_button_request_after_data_ack() {
    let mut chunk_request = trezor_client::protos::EthereumTxRequest::new();
    chunk_request.set_data_length(1);
    let button_request = trezor_client::protos::ButtonRequest::new();
    let mut final_request = trezor_client::protos::EthereumTxRequest::new();
    final_request.set_signature_v(1);
    final_request.set_signature_r(vec![1; 32]);
    final_request.set_signature_s(vec![2; 32]);

    let writes = Arc::new(Mutex::new(Vec::new()));
    let transport = QueuedTransport {
        responses: VecDeque::from([
            queued_message(&chunk_request),
            queued_message(&button_request),
            queued_message(&final_request),
        ]),
        writes: Arc::clone(&writes),
    };
    let client = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    let mut client = TrezorHardwareDerivationClient {
        client,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    let signature = client
        .sign_legacy_transaction(
            &[0x8000_002c, 0x8000_003c, 0x8000_0000, 0, 0],
            TrezorLegacySignRequest {
                nonce: vec![1],
                gas_price: vec![1],
                gas_limit: vec![0x52, 0x08],
                to: "0x1111111111111111111111111111111111111111".to_owned(),
                value: Vec::new(),
                data: vec![0xaa; TREZOR_ETHEREUM_TX_CHUNK_SIZE + 1],
                chain_id: Some(1),
            },
        )
        .expect("signing flow handles button request after ack");

    assert_eq!(signature.r(), U256::from_be_slice(&[1; 32]));
    assert_eq!(signature.s(), U256::from_be_slice(&[2; 32]));
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[
            MessageType::MessageType_EthereumSignTx,
            MessageType::MessageType_EthereumTxAck,
            MessageType::MessageType_ButtonAck,
        ]
    );
}

#[test]
fn trezor_pin_matrix_provider_continues_active_request() {
    let path = [0x8000_002c, 0x8000_003c, 0x8000_0000, 0, 0];
    let mut pin_request = trezor_client::protos::PinMatrixRequest::new();
    pin_request.set_type(
        trezor_client::protos::pin_matrix_request::PinMatrixRequestType::PinMatrixRequestType_Current,
    );
    let mut address = trezor_client::protos::EthereumAddress::new();
    address.set_address("0x1111111111111111111111111111111111111111".to_owned());

    let writes = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&pin_request), queued_message(&address)]),
        writes: Arc::clone(&writes),
    };
    let client = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    let mut client = TrezorHardwareDerivationClient {
        client,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    let provider_requests = Arc::clone(&requests);
    client.set_pin_matrix_provider(Arc::new(move |kind| {
        provider_requests
            .lock()
            .expect("provider requests lock")
            .push(kind);
        Ok(Zeroizing::new("123".to_owned()))
    }));

    let got = client
        .ethereum_address(&path)
        .expect("PIN matrix request is acknowledged");

    assert_eq!(got, "0x1111111111111111111111111111111111111111");
    assert_eq!(
        requests.lock().expect("requests lock").as_slice(),
        &[TrezorPinMatrixRequestKind::Current]
    );
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[
            MessageType::MessageType_EthereumGetAddress,
            MessageType::MessageType_PinMatrixAck,
        ]
    );
}

#[test]
fn trezor_passphrase_ack_defaults_to_no_passphrase() {
    let mut state = TrezorPassphraseState::default();

    let ack = state
        .next_passphrase_ack(false)
        .expect("standard wallet passphrase ack");

    assert!(ack.has_passphrase());
    assert_eq!(ack.passphrase(), "");
    assert!(!ack.has_on_device());
}

#[test]
fn trezor_passphrase_ack_uses_on_device_mode() {
    let mut state = TrezorPassphraseState::default();
    state.set_mode(TrezorPassphraseMode::EnterOnTrezor);

    let ack = state
        .next_passphrase_ack(false)
        .expect("on-device passphrase ack");

    assert!(ack.has_on_device());
    assert!(ack.on_device());
    assert!(!ack.has_passphrase());
}

#[test]
fn trezor_passphrase_ack_uses_app_passphrase_once() {
    let mut state = TrezorPassphraseState::default();
    state.set_app_passphrase("app secret".to_owned());

    let ack = state
        .next_passphrase_ack(false)
        .expect("app passphrase ack");

    assert!(ack.has_passphrase());
    assert_eq!(ack.passphrase(), "app secret");
    assert!(!format!("{ack:?}").contains("app secret"));
    assert!(!ack.has_on_device());
    assert!(state.app_passphrase.is_none());
    assert!(matches!(
        state.next_passphrase_ack(false),
        Err(HardwareDerivationError::MissingTrezorAppPassphrase)
    ));
}

#[test]
fn trezor_passphrase_ack_respects_device_required_on_device_entry() {
    let mut state = TrezorPassphraseState::default();
    state.set_app_passphrase("unused secret".to_owned());

    let ack = state
        .next_passphrase_ack(true)
        .expect("device-required on-device passphrase ack");

    assert!(!ack.has_on_device());
    assert!(!ack.has_passphrase());
    assert!(state.app_passphrase.is_none());
}

#[test]
fn trezor_initialize_captures_and_expires_session_id() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let features = test_features(Some(vec![9, 8, 7]));
    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&features)]),
        writes: Arc::clone(&writes),
    };
    let mut raw = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    raw.init_device(Some(vec![1, 2, 3]))
        .expect("resume Trezor session");
    let client = TrezorHardwareDerivationClient {
        client: raw,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    assert_eq!(client.session_id(), Some(vec![9, 8, 7]));
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[MessageType::MessageType_Initialize]
    );

    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&test_features(None))]),
        writes: Arc::new(Mutex::new(Vec::new())),
    };
    let mut raw = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    raw.init_device(Some(vec![1, 2, 3]))
        .expect("expired Trezor session initializes without id");
    let client = TrezorHardwareDerivationClient {
        client: raw,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    assert_eq!(client.session_id(), None);
}

#[test]
fn trezor_device_info_preserves_unlocked_feature() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut features = test_features(None);
    features.set_unlocked(false);
    features.set_passphrase_always_on_device(true);
    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&features)]),
        writes: Arc::clone(&writes),
    };
    let mut raw = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    raw.init_device(None).expect("initialize Trezor");
    let client = TrezorHardwareDerivationClient {
        client: raw,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };

    let info = client.device_info().expect("device info");

    assert_eq!(info.unlocked, Some(false));
    assert!(info.passphrase_always_on_device);
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[MessageType::MessageType_Initialize]
    );
}

#[test]
fn trezor_address_confirmation_sets_display_flag() {
    let path = [0x8000_002c, 0x8000_003c, 0x8000_0000, 0, 0];

    let silent = trezor_ethereum_get_address_request(&path, false);
    assert_eq!(silent.address_n, path.to_vec());
    assert!(!silent.show_display());

    let confirmed = trezor_ethereum_get_address_request(&path, true);
    assert_eq!(confirmed.address_n, path.to_vec());
    assert!(confirmed.show_display());
}

#[test]
fn trezor_account_public_key_request_reads_account_path_without_display() {
    let path = [0x8000_002c, 0x8000_003c, 0x8000_0000, 0];

    let request = trezor_ethereum_get_public_key_request(&path);

    assert_eq!(request.address_n, path.to_vec());
    assert!(!request.show_display());
}

fn test_account_node() -> trezor_client::protos::HDNodeType {
    let mut compressed = vec![0x02 | (TEST_ACCOUNT_PUBLIC_KEY[64] & 1)];
    compressed.extend_from_slice(&TEST_ACCOUNT_PUBLIC_KEY[1..33]);
    let mut node = trezor_client::protos::HDNodeType::new();
    node.set_depth(4);
    node.set_fingerprint(0);
    node.set_child_num(0);
    node.set_chain_code(TEST_ACCOUNT_CHAIN_CODE.to_vec());
    node.set_public_key(compressed);
    node
}

#[test]
fn trezor_account_public_key_from_node_checks_lengths() {
    let node = test_account_node();

    let key = trezor_account_public_key_from_node(&node).expect("account key");
    assert_eq!(key.public_key, node.public_key());
    assert_eq!(key.chain_code, TEST_ACCOUNT_CHAIN_CODE);

    let mut short_chain_code = node.clone();
    short_chain_code.set_chain_code(TEST_ACCOUNT_CHAIN_CODE[..31].to_vec());
    assert!(matches!(
        trezor_account_public_key_from_node(&short_chain_code),
        Err(HardwareDerivationError::UnexpectedHardwareResponse(_))
    ));
    let mut uncompressed = node;
    uncompressed.set_public_key(TEST_ACCOUNT_PUBLIC_KEY.to_vec());
    assert!(matches!(
        trezor_account_public_key_from_node(&uncompressed),
        Err(HardwareDerivationError::UnexpectedHardwareResponse(_))
    ));
}

#[test]
fn trezor_account_public_key_answers_pin_matrix_request() {
    let mut pin_request = trezor_client::protos::PinMatrixRequest::new();
    pin_request.set_type(
        trezor_client::protos::pin_matrix_request::PinMatrixRequestType::PinMatrixRequestType_Current,
    );
    let mut public_key = trezor_client::protos::EthereumPublicKey::new();
    public_key.node = Some(test_account_node()).into();
    public_key.set_xpub("xpub-account-key-marker".to_owned());

    let writes = Arc::new(Mutex::new(Vec::new()));
    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&pin_request), queued_message(&public_key)]),
        writes: Arc::clone(&writes),
    };
    let client = trezor_client::client::trezor_with_transport(
        trezor_client::Model::Trezor,
        Box::new(transport),
    );
    let mut client = TrezorHardwareDerivationClient {
        client,
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    client.set_pin_matrix_provider(Arc::new(
        |_: TrezorPinMatrixRequestKind| -> Result<_, HardwareDerivationError> {
            Ok(Zeroizing::new("123".to_owned()))
        },
    ));

    // The account key links every derived address, so even trace logs must not carry it.
    let (key, logs) = capture_trace_logs(|| client.account_public_key(0));
    let key = key.expect("PIN matrix request is acknowledged");
    assert!(!logs.contains("xpub-account-key-marker"));
    assert!(!logs.contains("EthereumPublicKey"));

    assert_eq!(key.public_key, test_account_node().public_key());
    assert_eq!(key.chain_code, TEST_ACCOUNT_CHAIN_CODE);
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[
            MessageType::MessageType_EthereumGetPublicKey,
            MessageType::MessageType_PinMatrixAck,
        ]
    );
}

#[test]
fn trezor_cipher_key_value_keeps_wallet_key_material_out_of_trace_logs() {
    let mut pin_request = trezor_client::protos::PinMatrixRequest::new();
    pin_request.set_type(
        trezor_client::protos::pin_matrix_request::PinMatrixRequestType::PinMatrixRequestType_Current,
    );
    let mut ciphered = trezor_client::protos::CipheredKeyValue::new();
    ciphered.set_value(vec![0xa5; 32]);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let transport = QueuedTransport {
        responses: VecDeque::from([queued_message(&pin_request), queued_message(&ciphered)]),
        writes: Arc::clone(&writes),
    };
    let mut client = TrezorHardwareDerivationClient {
        client: trezor_client::client::trezor_with_transport(
            trezor_client::Model::Trezor,
            Box::new(transport),
        ),
        passphrase: TrezorPassphraseState::default(),
        pin_matrix_provider: None,
    };
    client.set_pin_matrix_provider(Arc::new(
        |_: TrezorPinMatrixRequestKind| -> Result<_, HardwareDerivationError> {
            Ok(Zeroizing::new("123".to_owned()))
        },
    ));
    let descriptor = HardwareDerivationDescriptor::trezor_cipher_key_value_v1(
        crate::hardware::parse_bip32_path("m/44'/60'/0'/0/0").expect("valid path"),
        0,
        "trezor-profile-fingerprint".to_owned(),
        HardwareWalletSyncIntent::CreateNew,
    );

    // The output is the private wallet's entropy source, so even trace logs must not carry it.
    let (output, logs) = capture_trace_logs(|| client.cipher_key_value(&descriptor));

    assert_eq!(
        output.expect("derivation succeeds").expose_secret(),
        &[0xa5; 32]
    );
    assert!(!logs.contains("CipheredKeyValue"));
    assert!(!logs.contains("PinMatrixAck"));
    assert_eq!(
        writes.lock().expect("writes lock").as_slice(),
        &[
            MessageType::MessageType_CipherKeyValue,
            MessageType::MessageType_PinMatrixAck,
        ]
    );
}

#[test]
fn trezor_typed_data_struct_ack_encodes_members() {
    let model = typed_data_model();

    let ack = trezor_typed_data_struct_ack(&model, "Message").expect("struct ack");

    assert_eq!(ack.members.len(), 5);
    assert_eq!(ack.members[0].name(), "count");
    assert_eq!(
        ack.members[0]
            .type_
            .as_ref()
            .expect("count type")
            .data_type(),
        TrezorEthereumDataType::UINT
    );
    assert_eq!(
        ack.members[0].type_.as_ref().expect("count type").size(),
        32
    );
    assert_eq!(ack.members[1].name(), "person");
    let person_type = ack.members[1].type_.as_ref().expect("person type");
    assert_eq!(person_type.data_type(), TrezorEthereumDataType::STRUCT);
    assert_eq!(person_type.size(), 2);
    assert_eq!(person_type.struct_name(), "Person");
    let tags_type = ack.members[2].type_.as_ref().expect("tags type");
    assert_eq!(tags_type.data_type(), TrezorEthereumDataType::ARRAY);
    assert_eq!(tags_type.size(), 2);
    assert_eq!(
        tags_type
            .entry_type
            .as_ref()
            .expect("tags entry type")
            .data_type(),
        TrezorEthereumDataType::STRING
    );
}

#[test]
fn trezor_typed_data_value_ack_resolves_member_paths() {
    let model = typed_data_model();

    assert_eq!(
        trezor_typed_data_value(&model, &[0, 0]).expect("domain name"),
        b"RailOxide"
    );
    let chain_id = trezor_typed_data_value(&model, &[0, 1]).expect("chain id");
    assert_eq!(chain_id.len(), 32);
    assert_eq!(chain_id[31], 1);
    let count = trezor_typed_data_value(&model, &[1, 0]).expect("message count");
    assert_eq!(count.len(), 32);
    assert_eq!(count[31], 128);
    assert_eq!(
        trezor_typed_data_value(&model, &[1, 1, 0]).expect("nested address"),
        [0x11; 20]
    );
    assert_eq!(
        trezor_typed_data_value(&model, &[1, 2]).expect("array length"),
        [0, 2]
    );
    assert_eq!(
        trezor_typed_data_value(&model, &[1, 2, 1]).expect("array item"),
        b"test"
    );
    assert_eq!(
        trezor_typed_data_value(&model, &[1, 3]).expect("payload bytes"),
        [1, 2, 3]
    );
    assert_eq!(
        trezor_typed_data_value(&model, &[1, 4]).expect("fixed bytes"),
        [0xaa; 32]
    );
}

#[test]
fn trezor_typed_data_domain_values_follow_canonical_hash_order() {
    let model = reordered_domain_typed_data_model();
    let domain_ack = trezor_typed_data_struct_ack(&model, "EIP712Domain").expect("domain ack");

    assert_eq!(domain_ack.members[0].name(), "name");
    assert_eq!(domain_ack.members[1].name(), "chainId");
    assert_eq!(
        trezor_typed_data_value(&model, &[0, 0]).expect("domain name"),
        b"RailOxide"
    );
    let chain_id = trezor_typed_data_value(&model, &[0, 1]).expect("chain id");
    assert_eq!(chain_id.len(), 32);
    assert_eq!(chain_id[31], 1);
}

#[test]
fn trezor_clear_typed_data_flow_answers_struct_and_value_requests() {
    let model = typed_data_model();
    let descriptor = trezor_descriptor();
    let mut domain_request = trezor_client::protos::EthereumTypedDataStructRequest::new();
    domain_request.set_name("EIP712Domain".to_owned());
    let mut message_request = trezor_client::protos::EthereumTypedDataStructRequest::new();
    message_request.set_name("Message".to_owned());
    let mut value_request = trezor_client::protos::EthereumTypedDataValueRequest::new();
    value_request.member_path = vec![1, 0];
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut client = recording_client(
        VecDeque::from([
            queued_message(&domain_request),
            queued_message(&message_request),
            queued_message(&value_request),
            queued_message(&typed_data_signature()),
        ]),
        Arc::clone(&writes),
    );

    let signature = client
        .sign_typed_data_clear(&descriptor, &model)
        .expect("clear typed-data signature");

    assert_eq!(signature.r(), U256::from_be_slice(&[1; 32]));
    assert_eq!(signature.s(), U256::from_be_slice(&[2; 32]));
    let writes = writes.lock().expect("writes lock");
    assert_eq!(
        writes
            .iter()
            .map(|(message_type, _)| *message_type)
            .collect::<Vec<_>>(),
        vec![
            MessageType::MessageType_EthereumSignTypedData,
            MessageType::MessageType_EthereumTypedDataStructAck,
            MessageType::MessageType_EthereumTypedDataStructAck,
            MessageType::MessageType_EthereumTypedDataValueAck,
        ]
    );
    let request: trezor_client::protos::EthereumSignTypedData =
        protobuf::Message::parse_from_bytes(&writes[0].1).expect("sign typed-data request");
    assert_eq!(request.address_n, descriptor.path);
    assert_eq!(request.primary_type(), model.primary_type());
    assert!(request.metamask_v4_compat());
    let value_ack: trezor_client::protos::EthereumTypedDataValueAck =
        protobuf::Message::parse_from_bytes(&writes[3].1).expect("value ack");
    assert_eq!(value_ack.value().len(), 32);
    assert_eq!(value_ack.value()[31], 128);
}

#[test]
fn trezor_hash_typed_data_request_uses_local_hashes() {
    let model = typed_data_model();
    let descriptor = trezor_descriptor();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut client = recording_client(
        VecDeque::from([queued_message(&typed_data_signature())]),
        Arc::clone(&writes),
    );

    let signature = client
        .sign_typed_data_hash(&descriptor, &model)
        .expect("hash typed-data signature");

    assert_eq!(signature.r(), U256::from_be_slice(&[1; 32]));
    let writes = writes.lock().expect("writes lock");
    assert_eq!(writes[0].0, MessageType::MessageType_EthereumSignTypedHash);
    let request: trezor_client::protos::EthereumSignTypedHash =
        protobuf::Message::parse_from_bytes(&writes[0].1).expect("hash request");
    assert_eq!(request.address_n, descriptor.path);
    assert_eq!(
        request.domain_separator_hash(),
        model.domain_separator_hash().as_slice()
    );
    assert_eq!(
        request.message_hash(),
        model.message_hash().expect("message hash").as_slice()
    );
}

#[test]
fn trezor_typed_data_raw_loop_handles_passphrase_pin_and_button() {
    let model = typed_data_model();
    let descriptor = trezor_descriptor();
    let mut passphrase_request = trezor_client::protos::PassphraseRequest::new();
    passphrase_request.set__on_device(false);
    let mut pin_request = trezor_client::protos::PinMatrixRequest::new();
    pin_request.set_type(
        trezor_client::protos::pin_matrix_request::PinMatrixRequestType::PinMatrixRequestType_Current,
    );
    let button_request = trezor_client::protos::ButtonRequest::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let pin_requests = Arc::new(Mutex::new(Vec::new()));
    let mut client = recording_client(
        VecDeque::from([
            queued_message(&passphrase_request),
            queued_message(&pin_request),
            queued_message(&button_request),
            queued_message(&typed_data_signature()),
        ]),
        Arc::clone(&writes),
    );
    client.set_app_passphrase("app secret".to_owned());
    let recorded_pin_requests = Arc::clone(&pin_requests);
    client.set_pin_matrix_provider(Arc::new(move |kind| {
        recorded_pin_requests
            .lock()
            .expect("pin requests lock")
            .push(kind);
        Ok(Zeroizing::new("123".to_owned()))
    }));

    let signature = client
        .sign_typed_data_hash(&descriptor, &model)
        .expect("typed-data hash handles interactions");

    assert_eq!(signature.r(), U256::from_be_slice(&[1; 32]));
    assert_eq!(
        pin_requests.lock().expect("pin requests lock").as_slice(),
        &[TrezorPinMatrixRequestKind::Current]
    );
    assert!(client.passphrase.app_passphrase.is_none());
    let writes = writes.lock().expect("writes lock");
    assert_eq!(
        writes
            .iter()
            .map(|(message_type, _)| *message_type)
            .collect::<Vec<_>>(),
        vec![
            MessageType::MessageType_EthereumSignTypedHash,
            MessageType::MessageType_PassphraseAck,
            MessageType::MessageType_PinMatrixAck,
            MessageType::MessageType_ButtonAck,
        ]
    );
}

#[test]
fn trezor_typed_data_failure_response_is_preserved() {
    let model = typed_data_model();
    let descriptor = trezor_descriptor();
    let mut failure = trezor_client::protos::Failure::new();
    failure.set_code(trezor_client::protos::failure::FailureType::Failure_ActionCancelled);
    failure.set_message("cancelled".to_owned());
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut client = recording_client(
        VecDeque::from([queued_message(&failure)]),
        Arc::clone(&writes),
    );

    let error = client
        .sign_typed_data_hash(&descriptor, &model)
        .expect_err("failure response should fail");

    assert!(matches!(
        error,
        HardwareDerivationError::Trezor(trezor_client::Error::FailureResponse(_))
    ));
}

fn test_device_info(model: &str, version: HardwareAppVersion) -> TrezorDeviceInfo {
    TrezorDeviceInfo {
        model: model.to_owned(),
        vendor: "trezor.io".to_owned(),
        version,
        initialized: true,
        unlocked: Some(true),
        passphrase_protection: false,
        passphrase_always_on_device: false,
        bootloader_mode: false,
    }
}

#[test]
fn trezor_typed_data_classification_is_conservative() {
    let clear = test_device_info("T", HardwareAppVersion::new(2, 4, 3));
    let safe = test_device_info("T3T1", HardwareAppVersion::new(2, 7, 0));
    let fallback = test_device_info("1", HardwareAppVersion::new(1, 11, 2));
    let old_model_one = test_device_info("1", HardwareAppVersion::new(1, 11, 1));
    let unknown = test_device_info("unknown", HardwareAppVersion::new(2, 8, 0));

    assert_eq!(
        classify_trezor_typed_data_signing_mode(&clear),
        HardwareTypedDataSigningMode::ClearSign
    );
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&safe),
        HardwareTypedDataSigningMode::ClearSign
    );
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&fallback),
        HardwareTypedDataSigningMode::Eip712HashFallback
    );
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&old_model_one),
        HardwareTypedDataSigningMode::Unsupported
    );
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&unknown),
        HardwareTypedDataSigningMode::Unsupported
    );
}

#[test]
fn trezor_typed_data_classification_rejects_bootloader_or_uninitialized() {
    let mut info = test_device_info("T", HardwareAppVersion::new(2, 8, 0));
    info.bootloader_mode = true;
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&info),
        HardwareTypedDataSigningMode::Unsupported
    );

    info.bootloader_mode = false;
    info.initialized = false;
    assert_eq!(
        classify_trezor_typed_data_signing_mode(&info),
        HardwareTypedDataSigningMode::Unsupported
    );
}
