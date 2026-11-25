use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use alloy::dyn_abi::{DynSolType, DynSolValue, TypedData};
use alloy::primitives::{Address, B256, I256, U256};
use alloy::sol_types::Eip712Domain;
use serde_json::Value;
use thiserror::Error;

#[derive(Clone)]
#[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
pub(crate) struct HardwareEip712Model {
    typed_data: TypedData,
    primary_type: String,
    type_definitions: BTreeMap<String, Vec<HardwareEip712FieldDefinition>>,
    domain: HardwareEip712StructValue,
    message: Option<HardwareEip712StructValue>,
    signing_hash: B256,
    domain_separator_hash: B256,
    message_hash: Option<B256>,
}

impl fmt::Debug for HardwareEip712Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HardwareEip712Model(<redacted>)")
    }
}

impl HardwareEip712Model {
    pub(crate) fn from_walletconnect_typed_data_json(
        value: Value,
    ) -> Result<Self, HardwareEip712Error> {
        let mut type_definitions = parse_type_definitions(&value)?;
        let typed_data: TypedData = serde_json::from_value(value).map_err(|_| {
            HardwareEip712Error::InvalidPayload("payload is not valid EIP-712 JSON")
        })?;
        normalize_domain_definition_order(&typed_data.domain, &mut type_definitions)?;
        let signing_hash = typed_data.eip712_signing_hash().map_err(|_| {
            HardwareEip712Error::InvalidPayload("payload cannot produce an EIP-712 signing hash")
        })?;
        let domain_separator_hash = typed_data.domain.separator();
        let message_hash = if typed_data.primary_type == Eip712Domain::NAME {
            None
        } else {
            Some(typed_data.hash_struct().map_err(|_| {
                HardwareEip712Error::InvalidPayload("payload cannot produce a message hash")
            })?)
        };
        let domain = domain_value(
            &typed_data.domain,
            type_definitions.get(Eip712Domain::NAME).map(Vec::as_slice),
        )?;
        let message = if typed_data.primary_type == Eip712Domain::NAME {
            None
        } else {
            let message_type = typed_data
                .resolver
                .resolve(&typed_data.primary_type)
                .map_err(|_| {
                    HardwareEip712Error::InvalidPayload(
                        "primary typed-data type cannot be resolved",
                    )
                })?;
            let message_value = typed_data.coerce().map_err(|_| {
                HardwareEip712Error::InvalidPayload("message values do not match typed-data types")
            })?;
            Some(struct_value_from_dyn(&message_type, &message_value)?)
        };

        Ok(Self {
            primary_type: typed_data.primary_type.clone(),
            typed_data,
            type_definitions,
            domain,
            message,
            signing_hash,
            domain_separator_hash,
            message_hash,
        })
    }

    pub(crate) const fn typed_data(&self) -> &TypedData {
        &self.typed_data
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) fn primary_type(&self) -> &str {
        &self.primary_type
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) const fn type_definitions(
        &self,
    ) -> &BTreeMap<String, Vec<HardwareEip712FieldDefinition>> {
        &self.type_definitions
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) const fn domain(&self) -> &HardwareEip712StructValue {
        &self.domain
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) const fn message(&self) -> Option<&HardwareEip712StructValue> {
        self.message.as_ref()
    }

    pub(crate) const fn signing_hash(&self) -> B256 {
        self.signing_hash
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) const fn domain_separator_hash(&self) -> B256 {
        self.domain_separator_hash
    }

    #[cfg(any(test, feature = "hardware"))]
    pub(crate) const fn message_hash(&self) -> Option<B256> {
        self.message_hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardwareEip712FieldDefinition {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) value_type: HardwareEip712Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HardwareEip712Type {
    Primitive(HardwareEip712PrimitiveType),
    Struct(String),
    DynamicArray(Box<Self>),
    FixedArray { element: Box<Self>, len: usize },
}

impl HardwareEip712Type {
    fn parse(
        type_name: &str,
        struct_names: &BTreeSet<String>,
    ) -> Result<Self, HardwareEip712Error> {
        let (base, arrays) = split_array_type(type_name)?;
        let mut value_type = parse_base_type(base, struct_names)?;
        for array in arrays {
            value_type = match array {
                ArrayType::Dynamic => Self::DynamicArray(Box::new(value_type)),
                ArrayType::Fixed(len) => Self::FixedArray {
                    element: Box::new(value_type),
                    len,
                },
            };
        }
        Ok(value_type)
    }

    fn from_dyn(value_type: &DynSolType) -> Result<Self, HardwareEip712Error> {
        match value_type {
            DynSolType::Bool => Ok(Self::Primitive(HardwareEip712PrimitiveType::Bool)),
            DynSolType::Int(bits) => Ok(Self::Primitive(HardwareEip712PrimitiveType::Int(*bits))),
            DynSolType::Uint(bits) => Ok(Self::Primitive(HardwareEip712PrimitiveType::Uint(*bits))),
            DynSolType::FixedBytes(size) => Ok(Self::Primitive(
                HardwareEip712PrimitiveType::FixedBytes(*size),
            )),
            DynSolType::Address => Ok(Self::Primitive(HardwareEip712PrimitiveType::Address)),
            DynSolType::Bytes => Ok(Self::Primitive(HardwareEip712PrimitiveType::Bytes)),
            DynSolType::String => Ok(Self::Primitive(HardwareEip712PrimitiveType::String)),
            DynSolType::Array(element) => {
                Ok(Self::DynamicArray(Box::new(Self::from_dyn(element)?)))
            }
            DynSolType::FixedArray(element, len) => Ok(Self::FixedArray {
                element: Box::new(Self::from_dyn(element)?),
                len: *len,
            }),
            DynSolType::CustomStruct { name, .. } => Ok(Self::Struct(name.clone())),
            DynSolType::Function | DynSolType::Tuple(_) => Err(
                HardwareEip712Error::UnsupportedType(type_name_from_dyn(value_type)),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HardwareEip712PrimitiveType {
    Bool,
    Int(usize),
    Uint(usize),
    Address,
    FixedBytes(usize),
    Bytes,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardwareEip712StructValue {
    pub(crate) type_name: String,
    pub(crate) fields: Vec<HardwareEip712FieldValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardwareEip712FieldValue {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) value_type: HardwareEip712Type,
    pub(crate) value: HardwareEip712Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardwareEip712ArrayElement {
    pub(crate) value: HardwareEip712Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HardwareEip712Value {
    Bool(bool),
    Int { value: I256, bits: usize },
    Uint { value: U256, bits: usize },
    Address(Address),
    FixedBytes { bytes: Vec<u8>, size: usize },
    Bytes(Vec<u8>),
    String(String),
    Struct(HardwareEip712StructValue),
    DynamicArray(Vec<HardwareEip712ArrayElement>),
    FixedArray(Vec<HardwareEip712ArrayElement>),
}

#[derive(Debug, Error, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "hardware")), allow(dead_code))]
pub(crate) enum HardwareEip712Error {
    #[error("invalid EIP-712 typed-data payload: {0}")]
    InvalidPayload(&'static str),
    #[error("unsupported EIP-712 typed-data type: {0}")]
    UnsupportedType(String),
    #[error("unsupported EIP-712 typed-data value shape: {0}")]
    UnsupportedValueShape(&'static str),
    #[error("unsafe hardware EIP-712 hash fallback is unavailable")]
    #[allow(dead_code)]
    UnsafeHashFallback,
}

fn parse_type_definitions(
    payload: &Value,
) -> Result<BTreeMap<String, Vec<HardwareEip712FieldDefinition>>, HardwareEip712Error> {
    let types = payload.get("types").and_then(Value::as_object).ok_or(
        HardwareEip712Error::InvalidPayload("types must be an object"),
    )?;
    let struct_names = types.keys().cloned().collect::<BTreeSet<_>>();
    let mut definitions = BTreeMap::new();

    for (type_name, fields) in types {
        let fields = fields
            .as_array()
            .ok_or(HardwareEip712Error::InvalidPayload(
                "type definition fields must be arrays",
            ))?;
        let mut parsed_fields = Vec::with_capacity(fields.len());
        for field in fields {
            let name = field.get("name").and_then(Value::as_str).ok_or(
                HardwareEip712Error::InvalidPayload("type definition field names must be strings"),
            )?;
            let field_type = field.get("type").and_then(Value::as_str).ok_or(
                HardwareEip712Error::InvalidPayload("type definition field types must be strings"),
            )?;
            parsed_fields.push(HardwareEip712FieldDefinition {
                name: name.to_owned(),
                type_name: field_type.to_owned(),
                value_type: HardwareEip712Type::parse(field_type, &struct_names)?,
            });
        }
        definitions.insert(type_name.clone(), parsed_fields);
    }

    Ok(definitions)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayType {
    Dynamic,
    Fixed(usize),
}

fn split_array_type(type_name: &str) -> Result<(&str, Vec<ArrayType>), HardwareEip712Error> {
    let Some(array_start) = type_name.find('[') else {
        return Ok((type_name, Vec::new()));
    };
    let base = &type_name[..array_start];
    let mut arrays = Vec::new();
    let mut rest = &type_name[array_start..];
    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return Err(HardwareEip712Error::UnsupportedType(type_name.to_owned()));
        }
        let close = rest
            .find(']')
            .ok_or_else(|| HardwareEip712Error::UnsupportedType(type_name.to_owned()))?;
        let len = &rest[1..close];
        arrays.push(if len.is_empty() {
            ArrayType::Dynamic
        } else {
            ArrayType::Fixed(
                len.parse()
                    .map_err(|_| HardwareEip712Error::UnsupportedType(type_name.to_owned()))?,
            )
        });
        rest = &rest[close + 1..];
    }
    Ok((base, arrays))
}

fn parse_base_type(
    base: &str,
    struct_names: &BTreeSet<String>,
) -> Result<HardwareEip712Type, HardwareEip712Error> {
    let primitive_error = match parse_base_primitive_type(base) {
        Some(Ok(primitive)) => return Ok(HardwareEip712Type::Primitive(primitive)),
        Some(Err(error)) => Some(error),
        _ => None,
    };

    if struct_names.contains(base) {
        return Ok(HardwareEip712Type::Struct(base.to_owned()));
    }
    if let Some(error) = primitive_error {
        return Err(error);
    }
    Err(HardwareEip712Error::UnsupportedType(base.to_owned()))
}

fn parse_base_primitive_type(
    base: &str,
) -> Option<Result<HardwareEip712PrimitiveType, HardwareEip712Error>> {
    match base {
        "bool" => Some(Ok(HardwareEip712PrimitiveType::Bool)),
        "address" => Some(Ok(HardwareEip712PrimitiveType::Address)),
        "bytes" => Some(Ok(HardwareEip712PrimitiveType::Bytes)),
        "string" => Some(Ok(HardwareEip712PrimitiveType::String)),
        "int" => Some(Ok(HardwareEip712PrimitiveType::Int(256))),
        "uint" => Some(Ok(HardwareEip712PrimitiveType::Uint(256))),
        _ if base.starts_with("int") => {
            Some(parse_int_bits(base, "int").map(HardwareEip712PrimitiveType::Int))
        }
        _ if base.starts_with("uint") => {
            Some(parse_int_bits(base, "uint").map(HardwareEip712PrimitiveType::Uint))
        }
        _ if base.starts_with("bytes") => {
            Some(parse_fixed_bytes_size(base).map(HardwareEip712PrimitiveType::FixedBytes))
        }
        _ => None,
    }
}

fn parse_int_bits(type_name: &str, prefix: &str) -> Result<usize, HardwareEip712Error> {
    let bits = type_name[prefix.len()..]
        .parse::<usize>()
        .map_err(|_| HardwareEip712Error::UnsupportedType(type_name.to_owned()))?;
    if (8..=256).contains(&bits) && bits.is_multiple_of(8) {
        Ok(bits)
    } else {
        Err(HardwareEip712Error::UnsupportedType(type_name.to_owned()))
    }
}

fn parse_fixed_bytes_size(type_name: &str) -> Result<usize, HardwareEip712Error> {
    let size = type_name["bytes".len()..]
        .parse::<usize>()
        .map_err(|_| HardwareEip712Error::UnsupportedType(type_name.to_owned()))?;
    if (1..=32).contains(&size) {
        Ok(size)
    } else {
        Err(HardwareEip712Error::UnsupportedType(type_name.to_owned()))
    }
}

fn domain_value(
    domain: &Eip712Domain,
    definitions: Option<&[HardwareEip712FieldDefinition]>,
) -> Result<HardwareEip712StructValue, HardwareEip712Error> {
    let fields = if let Some(definitions) = definitions {
        domain_fields_from_definitions(domain, definitions)?
    } else {
        canonical_domain_fields(domain)
    };

    Ok(HardwareEip712StructValue {
        type_name: Eip712Domain::NAME.to_owned(),
        fields,
    })
}

fn canonical_domain_fields(domain: &Eip712Domain) -> Vec<HardwareEip712FieldValue> {
    let mut fields = Vec::new();
    if let Some(name) = domain.name.as_ref() {
        fields.push(domain_field(
            "name",
            "string",
            HardwareEip712Type::Primitive(HardwareEip712PrimitiveType::String),
            HardwareEip712Value::String(name.to_string()),
        ));
    }
    if let Some(version) = domain.version.as_ref() {
        fields.push(domain_field(
            "version",
            "string",
            HardwareEip712Type::Primitive(HardwareEip712PrimitiveType::String),
            HardwareEip712Value::String(version.to_string()),
        ));
    }
    if let Some(chain_id) = domain.chain_id {
        fields.push(domain_field(
            "chainId",
            "uint256",
            HardwareEip712Type::Primitive(HardwareEip712PrimitiveType::Uint(256)),
            HardwareEip712Value::Uint {
                value: chain_id,
                bits: 256,
            },
        ));
    }
    if let Some(verifying_contract) = domain.verifying_contract {
        fields.push(domain_field(
            "verifyingContract",
            "address",
            HardwareEip712Type::Primitive(HardwareEip712PrimitiveType::Address),
            HardwareEip712Value::Address(verifying_contract),
        ));
    }
    if let Some(salt) = domain.salt {
        fields.push(domain_field(
            "salt",
            "bytes32",
            HardwareEip712Type::Primitive(HardwareEip712PrimitiveType::FixedBytes(32)),
            HardwareEip712Value::FixedBytes {
                bytes: salt.as_slice().to_vec(),
                size: 32,
            },
        ));
    }

    fields
}

fn domain_fields_from_definitions(
    domain: &Eip712Domain,
    definitions: &[HardwareEip712FieldDefinition],
) -> Result<Vec<HardwareEip712FieldValue>, HardwareEip712Error> {
    for definition in definitions {
        if !matches!(
            definition.name.as_str(),
            "name" | "version" | "chainId" | "verifyingContract" | "salt"
        ) {
            return Err(HardwareEip712Error::InvalidPayload(
                "domain type definition contains unsupported field",
            ));
        }
    }

    let mut fields = Vec::with_capacity(definitions.len());
    append_domain_field_from_definitions(domain, definitions, &mut fields, "name")?;
    append_domain_field_from_definitions(domain, definitions, &mut fields, "version")?;
    append_domain_field_from_definitions(domain, definitions, &mut fields, "chainId")?;
    append_domain_field_from_definitions(domain, definitions, &mut fields, "verifyingContract")?;
    append_domain_field_from_definitions(domain, definitions, &mut fields, "salt")?;
    Ok(fields)
}

fn append_domain_field_from_definitions(
    domain: &Eip712Domain,
    definitions: &[HardwareEip712FieldDefinition],
    fields: &mut Vec<HardwareEip712FieldValue>,
    name: &str,
) -> Result<(), HardwareEip712Error> {
    let mut matching_definition = None;
    for definition in definitions
        .iter()
        .filter(|definition| definition.name == name)
    {
        if matching_definition.replace(definition).is_some() {
            return Err(HardwareEip712Error::InvalidPayload(
                "domain type definition contains duplicate field",
            ));
        }
    }
    let Some(definition) = matching_definition else {
        if domain_field_is_present(domain, name) {
            return Err(HardwareEip712Error::InvalidPayload(
                "domain value is not declared in EIP712Domain",
            ));
        }
        return Ok(());
    };
    fields.push(domain_field_from_definition(domain, definition)?);
    Ok(())
}

fn domain_field_is_present(domain: &Eip712Domain, name: &str) -> bool {
    match name {
        "name" => domain.name.is_some(),
        "version" => domain.version.is_some(),
        "chainId" => domain.chain_id.is_some(),
        "verifyingContract" => domain.verifying_contract.is_some(),
        "salt" => domain.salt.is_some(),
        _ => false,
    }
}

fn normalize_domain_definition_order(
    domain: &Eip712Domain,
    type_definitions: &mut BTreeMap<String, Vec<HardwareEip712FieldDefinition>>,
) -> Result<(), HardwareEip712Error> {
    let Some(definitions) = type_definitions.get(Eip712Domain::NAME) else {
        return Ok(());
    };
    let fields = domain_fields_from_definitions(domain, definitions)?;
    let definitions = fields
        .into_iter()
        .map(|field| HardwareEip712FieldDefinition {
            name: field.name,
            type_name: field.type_name,
            value_type: field.value_type,
        })
        .collect();
    type_definitions.insert(Eip712Domain::NAME.to_owned(), definitions);
    Ok(())
}

fn domain_field_from_definition(
    domain: &Eip712Domain,
    definition: &HardwareEip712FieldDefinition,
) -> Result<HardwareEip712FieldValue, HardwareEip712Error> {
    match definition.name.as_str() {
        "name" => {
            ensure_domain_field_type(definition, HardwareEip712PrimitiveType::String)?;
            let value = domain
                .name
                .as_ref()
                .ok_or(HardwareEip712Error::InvalidPayload(
                    "domain field value is missing",
                ))?;
            Ok(domain_field(
                &definition.name,
                &definition.type_name,
                definition.value_type.clone(),
                HardwareEip712Value::String(value.to_string()),
            ))
        }
        "version" => {
            ensure_domain_field_type(definition, HardwareEip712PrimitiveType::String)?;
            let value = domain
                .version
                .as_ref()
                .ok_or(HardwareEip712Error::InvalidPayload(
                    "domain field value is missing",
                ))?;
            Ok(domain_field(
                &definition.name,
                &definition.type_name,
                definition.value_type.clone(),
                HardwareEip712Value::String(value.to_string()),
            ))
        }
        "chainId" => {
            ensure_domain_field_type(definition, HardwareEip712PrimitiveType::Uint(256))?;
            let value = domain.chain_id.ok_or(HardwareEip712Error::InvalidPayload(
                "domain field value is missing",
            ))?;
            Ok(domain_field(
                &definition.name,
                &definition.type_name,
                definition.value_type.clone(),
                HardwareEip712Value::Uint { value, bits: 256 },
            ))
        }
        "verifyingContract" => {
            ensure_domain_field_type(definition, HardwareEip712PrimitiveType::Address)?;
            let value = domain
                .verifying_contract
                .ok_or(HardwareEip712Error::InvalidPayload(
                    "domain field value is missing",
                ))?;
            Ok(domain_field(
                &definition.name,
                &definition.type_name,
                definition.value_type.clone(),
                HardwareEip712Value::Address(value),
            ))
        }
        "salt" => {
            ensure_domain_field_type(definition, HardwareEip712PrimitiveType::FixedBytes(32))?;
            let value = domain.salt.ok_or(HardwareEip712Error::InvalidPayload(
                "domain field value is missing",
            ))?;
            Ok(domain_field(
                &definition.name,
                &definition.type_name,
                definition.value_type.clone(),
                HardwareEip712Value::FixedBytes {
                    bytes: value.as_slice().to_vec(),
                    size: 32,
                },
            ))
        }
        _ => Err(HardwareEip712Error::InvalidPayload(
            "domain type definition contains unsupported field",
        )),
    }
}

fn ensure_domain_field_type(
    definition: &HardwareEip712FieldDefinition,
    expected: HardwareEip712PrimitiveType,
) -> Result<(), HardwareEip712Error> {
    if matches!(&definition.value_type, HardwareEip712Type::Primitive(actual) if actual == &expected)
    {
        Ok(())
    } else {
        Err(HardwareEip712Error::InvalidPayload(
            "domain type definition field type is invalid",
        ))
    }
}

fn domain_field(
    name: &str,
    type_name: &str,
    value_type: HardwareEip712Type,
    value: HardwareEip712Value,
) -> HardwareEip712FieldValue {
    HardwareEip712FieldValue {
        name: name.to_owned(),
        type_name: type_name.to_owned(),
        value_type,
        value,
    }
}

fn struct_value_from_dyn(
    value_type: &DynSolType,
    value: &DynSolValue,
) -> Result<HardwareEip712StructValue, HardwareEip712Error> {
    let DynSolType::CustomStruct {
        name,
        prop_names,
        tuple: prop_types,
    } = value_type
    else {
        return Err(HardwareEip712Error::UnsupportedValueShape(
            "root typed-data value must be a struct",
        ));
    };
    let DynSolValue::CustomStruct {
        tuple: prop_values, ..
    } = value
    else {
        return Err(HardwareEip712Error::UnsupportedValueShape(
            "typed-data value does not match struct type",
        ));
    };
    struct_value_from_parts(name, prop_names, prop_types, prop_values)
}

fn struct_value_from_parts(
    name: &str,
    prop_names: &[String],
    prop_types: &[DynSolType],
    prop_values: &[DynSolValue],
) -> Result<HardwareEip712StructValue, HardwareEip712Error> {
    if prop_names.len() != prop_types.len() || prop_names.len() != prop_values.len() {
        return Err(HardwareEip712Error::UnsupportedValueShape(
            "typed-data struct field count mismatch",
        ));
    }
    let fields = prop_names
        .iter()
        .zip(prop_types)
        .zip(prop_values)
        .map(|((field_name, field_type), field_value)| {
            Ok(HardwareEip712FieldValue {
                name: field_name.clone(),
                type_name: type_name_from_dyn(field_type),
                value_type: HardwareEip712Type::from_dyn(field_type)?,
                value: value_from_dyn(field_type, field_value)?,
            })
        })
        .collect::<Result<Vec<_>, HardwareEip712Error>>()?;

    Ok(HardwareEip712StructValue {
        type_name: name.to_owned(),
        fields,
    })
}

fn value_from_dyn(
    value_type: &DynSolType,
    value: &DynSolValue,
) -> Result<HardwareEip712Value, HardwareEip712Error> {
    match (value_type, value) {
        (DynSolType::Bool, DynSolValue::Bool(value)) => Ok(HardwareEip712Value::Bool(*value)),
        (DynSolType::Int(bits), DynSolValue::Int(value, value_bits)) if bits == value_bits => {
            Ok(HardwareEip712Value::Int {
                value: *value,
                bits: *bits,
            })
        }
        (DynSolType::Uint(bits), DynSolValue::Uint(value, value_bits)) if bits == value_bits => {
            Ok(HardwareEip712Value::Uint {
                value: *value,
                bits: *bits,
            })
        }
        (DynSolType::FixedBytes(size), DynSolValue::FixedBytes(value, value_size))
            if size == value_size =>
        {
            Ok(HardwareEip712Value::FixedBytes {
                bytes: value[..*size].to_vec(),
                size: *size,
            })
        }
        (DynSolType::Address, DynSolValue::Address(value)) => {
            Ok(HardwareEip712Value::Address(*value))
        }
        (DynSolType::Bytes, DynSolValue::Bytes(value)) => {
            Ok(HardwareEip712Value::Bytes(value.clone()))
        }
        (DynSolType::String, DynSolValue::String(value)) => {
            Ok(HardwareEip712Value::String(value.clone()))
        }
        (DynSolType::Array(element_type), DynSolValue::Array(values)) => {
            array_value(element_type, values, HardwareEip712Value::DynamicArray)
        }
        (DynSolType::FixedArray(element_type, expected_len), DynSolValue::FixedArray(values)) => {
            if values.len() != *expected_len {
                return Err(HardwareEip712Error::UnsupportedValueShape(
                    "fixed-array value length does not match type",
                ));
            }
            array_value(element_type, values, HardwareEip712Value::FixedArray)
        }
        (
            DynSolType::CustomStruct {
                name,
                prop_names,
                tuple,
            },
            DynSolValue::CustomStruct {
                tuple: prop_values, ..
            },
        ) => Ok(HardwareEip712Value::Struct(struct_value_from_parts(
            name,
            prop_names,
            tuple,
            prop_values,
        )?)),
        _ => Err(HardwareEip712Error::UnsupportedValueShape(
            "typed-data value does not match resolved type",
        )),
    }
}

fn array_value(
    element_type: &DynSolType,
    values: &[DynSolValue],
    wrap: impl FnOnce(Vec<HardwareEip712ArrayElement>) -> HardwareEip712Value,
) -> Result<HardwareEip712Value, HardwareEip712Error> {
    values
        .iter()
        .map(|value| {
            Ok(HardwareEip712ArrayElement {
                value: value_from_dyn(element_type, value)?,
            })
        })
        .collect::<Result<Vec<_>, HardwareEip712Error>>()
        .map(wrap)
}

fn type_name_from_dyn(value_type: &DynSolType) -> String {
    match value_type {
        DynSolType::Bool => "bool".to_owned(),
        DynSolType::Int(bits) => format!("int{bits}"),
        DynSolType::Uint(bits) => format!("uint{bits}"),
        DynSolType::FixedBytes(size) => format!("bytes{size}"),
        DynSolType::Address => "address".to_owned(),
        DynSolType::Function => "function".to_owned(),
        DynSolType::Bytes => "bytes".to_owned(),
        DynSolType::String => "string".to_owned(),
        DynSolType::Array(element) => format!("{}[]", type_name_from_dyn(element)),
        DynSolType::FixedArray(element, len) => format!("{}[{len}]", type_name_from_dyn(element)),
        DynSolType::Tuple(tuple) => {
            let inner = tuple
                .iter()
                .map(type_name_from_dyn)
                .collect::<Vec<_>>()
                .join(",");
            format!("({inner})")
        }
        DynSolType::CustomStruct { name, .. } => name.clone(),
    }
}

#[cfg(test)]
mod tests;
