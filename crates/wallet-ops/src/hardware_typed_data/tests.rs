use alloy::dyn_abi::TypedData;
use serde_json::{Value, json};

use super::*;

fn assert_hashes_match_alloy(payload: Value) -> HardwareEip712Model {
    let typed_data: TypedData = serde_json::from_value(payload.clone()).expect("typed data");
    let model = HardwareEip712Model::from_walletconnect_typed_data_json(payload)
        .expect("hardware typed-data model");

    assert_eq!(model.primary_type(), typed_data.primary_type);
    assert_eq!(
        model.signing_hash(),
        typed_data
            .eip712_signing_hash()
            .expect("Alloy signing hash")
    );
    assert_eq!(model.domain_separator_hash(), typed_data.domain.separator());
    if typed_data.primary_type == Eip712Domain::NAME {
        assert_eq!(model.message_hash(), None);
    } else {
        assert_eq!(
            model.message_hash(),
            Some(typed_data.hash_struct().expect("Alloy message hash"))
        );
    }

    model
}

#[test]
fn shared_model_hashes_and_traverses_nested_arrays_and_bytes() {
    let payload = json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" },
                { "name": "verifyingContract", "type": "address" },
                { "name": "salt", "type": "bytes32" }
            ],
            "Person": [
                { "name": "name", "type": "string" },
                { "name": "wallets", "type": "address[]" }
            ],
            "Attachment": [
                { "name": "data", "type": "bytes" },
                { "name": "digest", "type": "bytes32" }
            ],
            "Message": [
                { "name": "from", "type": "Person" },
                { "name": "to", "type": "Person[]" },
                { "name": "tags", "type": "string[2]" },
                { "name": "attachments", "type": "Attachment[]" },
                { "name": "emptyBytes", "type": "bytes" },
                { "name": "emptyList", "type": "uint256[]" },
                { "name": "salt", "type": "bytes32" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "version": "1",
            "chainId": 1,
            "verifyingContract": "0x0000000000000000000000000000000000000001",
            "salt": "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        },
        "message": {
            "from": {
                "name": "Alice",
                "wallets": [
                    "0x1111111111111111111111111111111111111111",
                    "0x2222222222222222222222222222222222222222"
                ]
            },
            "to": [
                {
                    "name": "Bob",
                    "wallets": ["0x3333333333333333333333333333333333333333"]
                }
            ],
            "tags": ["permit", ""],
            "attachments": [
                {
                    "data": "0x010203",
                    "digest": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                },
                {
                    "data": "0x",
                    "digest": "0x0000000000000000000000000000000000000000000000000000000000000000"
                }
            ],
            "emptyBytes": "0x",
            "emptyList": [],
            "salt": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    });

    let model = assert_hashes_match_alloy(payload);
    assert_eq!(model.type_definitions()["Message"].len(), 7);
    assert_eq!(model.domain().fields.len(), 5);
    let message = model.message().expect("message root");
    assert_eq!(message.type_name, "Message");

    let from = message
        .fields
        .iter()
        .find(|field| field.name == "from")
        .expect("from field");
    assert!(matches!(from.value_type, HardwareEip712Type::Struct(_)));
    assert!(matches!(from.value, HardwareEip712Value::Struct(_)));

    let to = message
        .fields
        .iter()
        .find(|field| field.name == "to")
        .expect("to field");
    assert!(matches!(to.value_type, HardwareEip712Type::DynamicArray(_)));
    assert!(matches!(to.value, HardwareEip712Value::DynamicArray(ref values) if values.len() == 1));

    let tags = message
        .fields
        .iter()
        .find(|field| field.name == "tags")
        .expect("tags field");
    assert!(matches!(
        tags.value_type,
        HardwareEip712Type::FixedArray { len: 2, .. }
    ));
    assert!(matches!(tags.value, HardwareEip712Value::FixedArray(ref values) if values.len() == 2));

    let empty_bytes = message
        .fields
        .iter()
        .find(|field| field.name == "emptyBytes")
        .expect("empty bytes field");
    assert!(matches!(empty_bytes.value, HardwareEip712Value::Bytes(ref bytes) if bytes.is_empty()));

    let empty_list = message
        .fields
        .iter()
        .find(|field| field.name == "emptyList")
        .expect("empty list field");
    assert!(
        matches!(empty_list.value, HardwareEip712Value::DynamicArray(ref values) if values.is_empty())
    );

    let salt = message
        .fields
        .iter()
        .find(|field| field.name == "salt")
        .expect("salt field");
    assert!(matches!(
        salt.value,
        HardwareEip712Value::FixedBytes { size: 32, .. }
    ));
}

#[test]
fn shared_model_accepts_custom_type_names_with_primitive_prefixes() {
    let payload = json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "bytesPayload": [
                { "name": "digest", "type": "bytes32" }
            ],
            "uintOrder": [
                { "name": "amount", "type": "uint256" }
            ],
            "intDetails": [
                { "name": "note", "type": "string" }
            ],
            "Message": [
                { "name": "payload", "type": "bytesPayload" },
                { "name": "order", "type": "uintOrder" },
                { "name": "details", "type": "intDetails" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "payload": {
                "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            },
            "order": {
                "amount": 5
            },
            "details": {
                "note": "custom"
            }
        }
    });

    let model = assert_hashes_match_alloy(payload);
    let message = model.message().expect("message root");

    for (field_name, type_name) in [
        ("payload", "bytesPayload"),
        ("order", "uintOrder"),
        ("details", "intDetails"),
    ] {
        let field = message
            .fields
            .iter()
            .find(|field| field.name == field_name)
            .expect("custom field");
        assert!(matches!(
            &field.value_type,
            HardwareEip712Type::Struct(name) if name == type_name
        ));
    }
}

#[test]
fn shared_model_supports_domain_only_payloads() {
    let payload = json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" },
                { "name": "verifyingContract", "type": "address" }
            ]
        },
        "primaryType": "EIP712Domain",
        "domain": {
            "name": "example.metamask.io",
            "version": "1",
            "chainId": 1,
            "verifyingContract": "0x0000000000000000000000000000000000000000"
        },
        "message": {}
    });

    let model = assert_hashes_match_alloy(payload);

    assert_eq!(model.primary_type(), Eip712Domain::NAME);
    assert_eq!(model.message(), None);
    assert_eq!(model.message_hash(), None);
    assert_eq!(model.domain().fields.len(), 4);
}

#[test]
fn shared_model_normalizes_domain_field_order_to_canonical_hash_order() {
    let payload = json!({
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
    });

    let model = assert_hashes_match_alloy(payload);

    let domain_definitions = &model.type_definitions()[Eip712Domain::NAME];
    assert_eq!(domain_definitions[0].name, "name");
    assert_eq!(domain_definitions[1].name, "chainId");
    assert_eq!(model.domain().fields[0].name, "name");
    assert!(matches!(
        model.domain().fields[0].value,
        HardwareEip712Value::String(ref value) if value == "RailOxide"
    ));
    assert_eq!(model.domain().fields[1].name, "chainId");
    assert!(matches!(
        model.domain().fields[1].value,
        HardwareEip712Value::Uint { value, bits: 256 } if value == alloy::primitives::U256::from(1_u64)
    ));
}

#[test]
fn unsupported_shape_errors_do_not_include_raw_values() {
    let payload = json!({
        "types": {
            "EIP712Domain": [],
            "Message": [
                { "name": "unsafe", "type": "function" }
            ]
        },
        "primaryType": "Message",
        "domain": {},
        "message": {
            "unsafe": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    });

    let Err(error) = HardwareEip712Model::from_walletconnect_typed_data_json(payload) else {
        panic!("unsupported function type was accepted")
    };
    let message = error.to_string();

    assert!(message.contains("function"));
    assert!(!message.contains("aaaaaaaa"));
}

#[test]
fn unsafe_fallback_error_does_not_include_sign_bytes() {
    let message = HardwareEip712Error::UnsafeHashFallback.to_string();

    assert!(message.contains("unsafe hardware"));
    assert!(!message.contains("0x"));
}
