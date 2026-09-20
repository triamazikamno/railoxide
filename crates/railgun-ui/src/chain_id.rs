//! JSON representation for application chain IDs crossing JavaScript.
//! Safe IDs retain compatibility with older extension builds; larger IDs are decimal strings.
use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

pub const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;

pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    if *value <= MAX_SAFE_INTEGER {
        serializer.serialize_u64(*value)
    } else {
        serializer.serialize_str(&value.to_string())
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(u64),
    }
    match Value::deserialize(deserializer)? {
        Value::Text(value) => value
            .parse()
            .map_err(|_| D::Error::custom("invalid decimal chain ID")),
        Value::Number(value) if value <= MAX_SAFE_INTEGER => Ok(value),
        Value::Number(_) => Err(D::Error::custom("large chain IDs must be decimal strings")),
    }
}

pub mod optional {
    use super::Serializer;
    use serde::Serialize;

    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Id(#[serde(serialize_with = "super::serialize")] u64);
        value.map(Id).serialize(serializer)
    }
}
