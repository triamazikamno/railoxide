use serde_json::{Value, json};

use crate::hardware_typed_data::HardwareEip712Model;
use crate::walletconnect::{
    WalletConnectError, WalletConnectParsedRequest, parse_walletconnect_session_request,
};

use super::helpers::{approved_request_session, typed_data_payload};

#[test]
fn rejects_typed_data_beyond_type_limits_before_approval() {
    let (_, account) = approved_request_session(&["eth_signTypedData_v4"]);
    let parse = |payload: Value| {
        parse_walletconnect_session_request(
            31,
            "eth_signTypedData_v4",
            &json!([account.address.to_string(), payload]),
        )
    };

    // Depth 64, weight 1,200.
    let at_limit = struct_chain_typed_data(64);
    assert!(matches!(
        parse(at_limit.clone()),
        Ok(WalletConnectParsedRequest::EthSignTypedDataV4 { .. })
    ));
    // Hashing and the hardware model walk the same depth, so the limit must be safe for them too.
    assert!(HardwareEip712Model::from_walletconnect_typed_data_json(at_limit).is_ok());

    for (case, payload) in over_limit_payloads() {
        assert!(
            matches!(
                parse(payload),
                Err(WalletConnectError::MalformedParams(message))
                    if message.contains("exceed the typed-data limits")
            ),
            "{case}"
        );
    }
}

/// Payloads alloy accepts that each cross one limit.
fn over_limit_payloads() -> Vec<(&'static str, Value)> {
    let array_dimensions = typed_data_with_array_field(&format!("uint256{}", "[]".repeat(64)));
    let double_encoded = Value::String(
        serde_json::to_string(&serde_json::to_string(&array_dimensions).unwrap()).unwrap(),
    );
    let mut primary_type_dimensions = typed_data_payload(&json!(1));
    primary_type_dimensions["primaryType"] = json!(format!("Message{}", "[]".repeat(64)));
    primary_type_dimensions["message"] = json!([]);
    // The resolver drops this key, but the hardware model parses its field.
    let mut unresolved_key = typed_data_payload(&json!(1));
    unresolved_key["types"]["Junk[]"] =
        json!([{ "name": "x", "type": format!("uint256{}", "[]".repeat(64)) }]);
    let mut duplicate_field_names = typed_data_payload(&json!(1));
    duplicate_field_names["types"]["Message"] = json!([
        { "name": "contents", "type": "string" },
        { "name": "contents", "type": "string" }
    ]);
    let long_name = "S".repeat(257);
    let mut long_type_name = typed_data_with_array_field(&format!("{long_name}[]"));
    long_type_name["types"][long_name] = json!([{ "name": "flag", "type": "bool" }]);
    // Parsing reads each `a.kN` as `uint8`, but alloy's cycle check follows the literal names.
    let mut dotted_chain = typed_data_payload(&json!(1));
    dotted_chain["types"]["Message"] = json!([{ "name": "next", "type": "a.k1" }]);
    for level in 1..65 {
        dotted_chain["types"][format!("a.k{level}")] =
            json!([{ "name": "next", "type": format!("a.k{}", level + 1) }]);
    }
    dotted_chain["message"] = json!({ "next": 1 });
    // alloy reparses the 4 KiB leaf type string for each of its 32 expansions.
    let mut padded_type = fan_out_typed_data(5);
    padded_type["types"]["Fan5"] =
        json!([{ "name": "value", "type": format!("uint8[{}1]", "0".repeat(4096)) }]);
    let mut large_types = typed_data_payload(&json!(1));
    large_types["types"]["Unused"] = (0..600)
        .map(|index| json!({ "name": format!("field{index}"), "type": "uint256" }))
        .collect();
    vec![
        ("array dimensions", array_dimensions),
        ("struct chain", struct_chain_typed_data(65)),
        (
            "tuple nesting",
            typed_data_with_array_field(&format!("{}uint256{}[]", "(".repeat(63), ")".repeat(63))),
        ),
        ("primaryType dimensions", primary_type_dimensions),
        ("double-encoded payload", double_encoded),
        ("unresolved type key", unresolved_key),
        // Depth 29, weight about 702,000.
        ("struct fan-out", fan_out_typed_data(14)),
        ("duplicate field names", duplicate_field_names),
        ("long type name", long_type_name),
        ("normalized name chain", dotted_chain),
        ("padded type string", padded_type),
        ("large types object", large_types),
    ]
}

/// A chain of `levels` structs, from `Message` down to one holding a `uint256`.
fn struct_chain_typed_data(levels: usize) -> Value {
    let name = |level: usize| match level {
        0 => "Message".to_owned(),
        _ => format!("Level{level}"),
    };
    let mut payload = typed_data_payload(&json!(1));
    payload["types"][name(levels - 1)] = json!([{ "name": "value", "type": "uint256" }]);
    let mut message = json!({ "value": 1 });
    for level in (0..levels - 1).rev() {
        payload["types"][name(level)] = json!([{ "name": "next", "type": name(level + 1) }]);
        message = json!({ "next": message });
    }
    payload["message"] = message;
    payload
}

/// `Message` and `Fan1` to `Fan{levels - 1}` each hold two arrays of the next struct, down to a
/// `Fan{levels}` holding a `uint256`.
fn fan_out_typed_data(levels: usize) -> Value {
    let mut payload = typed_data_payload(&json!(1));
    payload["types"][format!("Fan{levels}")] = json!([{ "name": "value", "type": "uint256" }]);
    for level in 0..levels {
        let name = match level {
            0 => "Message".to_owned(),
            _ => format!("Fan{level}"),
        };
        let child = format!("Fan{}[]", level + 1);
        payload["types"][name] =
            json!([{ "name": "a", "type": child }, { "name": "b", "type": child }]);
    }
    payload["message"] = json!({ "a": [], "b": [] });
    payload
}

/// `Message` holds one empty array of `field_type`.
fn typed_data_with_array_field(field_type: &str) -> Value {
    let mut payload = typed_data_payload(&json!(1));
    payload["types"]["Message"] = json!([{ "name": "items", "type": field_type }]);
    payload["message"] = json!({ "items": [] });
    payload
}
