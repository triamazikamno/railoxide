// shortcut: duplicates limits alloy-core added on main in #1188, #1189, #1190 and #1258; delete
// this module once a release after alloy-core 1.7.3 ships them.

use std::collections::{BTreeMap, BTreeSet};

use alloy::dyn_abi::Eip712Types;
use alloy::dyn_abi::parser::{TypeSpecifier, TypeStem};

// Real schemas are 1-2 KB; this bounds every cost alloy has that grows with the schema alone.
pub(super) const MAX_TYPED_DATA_TYPES_BYTES: usize = 16 * 1024;
// alloy-dyn-abi recurses once per type nesting level, and a stack overflow aborts the process.
pub(super) const MAX_TYPED_DATA_TYPE_DEPTH: usize = 64;
// alloy copies each struct reference in full, so expansion can grow exponentially.
pub(super) const MAX_TYPED_DATA_TYPE_WEIGHT: usize = 64 * 1024;
// alloy copies a struct's name into every coerced value of it, such as each array element.
const MAX_TYPED_DATA_TYPE_NAME_LEN: usize = 256;

/// Nesting depth and approximate cost of a type as alloy resolves it: copied names and reparsed
/// type strings.
#[derive(Clone, Copy)]
struct TypeSize {
    depth: usize,
    weight: usize,
}

impl TypeSize {
    const LEAF: Self = Self {
        depth: 0,
        weight: 1,
    };

    /// `None` once `weight` exceeds the limit.
    fn checked(depth: usize, weight: usize) -> Option<Self> {
        (weight <= MAX_TYPED_DATA_TYPE_WEIGHT).then_some(Self { depth, weight })
    }

    /// Adds a member and `extra` weight of its own, keeping the deepest depth.
    fn with_member(self, member: Self, extra: usize) -> Option<Self> {
        Self::checked(
            self.depth.max(member.depth),
            self.weight + extra + member.weight,
        )
    }
}

/// Checks every declared type, used or not, and the primary type expression.
pub(super) fn typed_data_types_within_limits(types: &Eip712Types, primary_type: &str) -> bool {
    if serde_json::to_vec(types).map_or(true, |json| json.len() > MAX_TYPED_DATA_TYPES_BYTES) {
        return false;
    }
    let mut struct_sizes = BTreeMap::new();
    types.keys().all(|name| {
        struct_type_size(types, &mut struct_sizes, name, MAX_TYPED_DATA_TYPE_DEPTH).is_some()
    }) && type_expression_size(
        types,
        &mut struct_sizes,
        primary_type,
        MAX_TYPED_DATA_TYPE_DEPTH,
    )
    .is_some()
}

/// Size of a type expression, or `None` once its depth exceeds `budget` or its weight the limit.
fn type_expression_size<'a>(
    types: &'a Eip712Types,
    struct_sizes: &mut BTreeMap<&'a str, TypeSize>,
    type_name: &str,
    budget: usize,
) -> Option<TypeSize> {
    // alloy rejects a type that fails to parse, during deserialization or in `coerce`.
    let Ok(specifier) = TypeSpecifier::parse_eip712(type_name) else {
        return Some(TypeSize::LEAF);
    };
    type_specifier_size(types, struct_sizes, &specifier, budget)
}

fn type_specifier_size<'a>(
    types: &'a Eip712Types,
    struct_sizes: &mut BTreeMap<&'a str, TypeSize>,
    specifier: &TypeSpecifier<'_>,
    budget: usize,
) -> Option<TypeSize> {
    let arrays = specifier.sizes.len();
    let stem_budget = budget.checked_sub(arrays)?;
    let stem = match &specifier.stem {
        TypeStem::Root(root) => struct_type_size(types, struct_sizes, root.span(), stem_budget)?,
        TypeStem::Tuple(tuple) => {
            let member_budget = stem_budget.checked_sub(1)?;
            let mut size = TypeSize::LEAF;
            for member in &tuple.types {
                let member = type_specifier_size(types, struct_sizes, member, member_budget)?;
                size = size.with_member(member, 0)?;
            }
            TypeSize {
                depth: size.depth + 1,
                ..size
            }
        }
    };
    TypeSize::checked(stem.depth + arrays, stem.weight + arrays)
}

/// Size of a declared struct (a leaf for any other root type), or `None` past either limit.
/// Cycles exhaust the budget. alloy rejects only cycles reachable from the primary type; this
/// also rejects an unused recursive type.
/// Duplicate field names are rejected too: alloy fills each from the same message key, so the
/// coerced value can grow far beyond the message.
fn struct_type_size<'a>(
    types: &'a Eip712Types,
    struct_sizes: &mut BTreeMap<&'a str, TypeSize>,
    name: &str,
    budget: usize,
) -> Option<TypeSize> {
    let Some((name, fields)) = types.get_key_value(name) else {
        return Some(TypeSize::LEAF);
    };
    if let Some(&size) = struct_sizes.get(name.as_str()) {
        return (size.depth <= budget).then_some(size);
    }
    if name.len() > MAX_TYPED_DATA_TYPE_NAME_LEN {
        return None;
    }
    let field_budget = budget.checked_sub(1)?;
    let mut size = TypeSize::checked(0, 1 + name.len())?;
    let mut field_names = BTreeSet::new();
    for field in fields {
        if !field_names.insert(field.name()) {
            return None;
        }
        let mut field_size =
            type_expression_size(types, struct_sizes, field.type_name(), field_budget)?;
        // alloy's cycle check follows the literal name before `[`, which parsing can normalize
        // (`uint` to `uint256`, any `A.B` to `uint8`).
        let edge = struct_type_size(types, struct_sizes, field.root_type_name(), field_budget)?;
        field_size.depth = field_size.depth.max(edge.depth);
        // alloy reparses the field's type string each time it expands this struct.
        size = size.with_member(field_size, field.name().len() + field.type_name().len())?;
    }
    let size = TypeSize {
        depth: size.depth + 1,
        ..size
    };
    struct_sizes.insert(name.as_str(), size);
    Some(size)
}
