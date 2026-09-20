use super::{Address, FromStr, Url, WakuDirectPeerSetting, hex, parse_multiaddr, parse_peer_id};

pub(super) fn validate_address(field: &str, value: &str, errors: &mut Vec<String>) {
    if Address::from_str(value).is_err() {
        errors.push(format!("{field} must be an EVM address"));
    }
}

pub(super) fn validate_optional_address(
    field: &str,
    value: Option<&str>,
    errors: &mut Vec<String>,
) {
    if let Some(value) = value {
        validate_address(field, value, errors);
    }
}

pub(super) fn validate_optional_non_empty(
    field: &str,
    value: Option<&str>,
    errors: &mut Vec<String>,
) {
    if value.is_some_and(|value| value.trim().is_empty()) {
        errors.push(format!("{field} must not be empty"));
    }
}

pub(super) fn validate_url_scheme(
    field: &str,
    value: &str,
    schemes: &[&str],
    errors: &mut Vec<String>,
) {
    match Url::parse(value) {
        Ok(url) if schemes.contains(&url.scheme()) => {}
        Ok(url) => errors.push(format!(
            "{field} must use one of these URL schemes: {}; got {}",
            schemes.join(", "),
            url.scheme()
        )),
        Err(error) => errors.push(format!("{field} is not a valid URL: {error}")),
    }
}

pub(super) fn validate_enr_tree(field: &str, value: &str, errors: &mut Vec<String>) {
    let value = value.trim();
    if value.is_empty() {
        errors.push(format!("{field} must not be empty"));
    } else if !value.starts_with("enrtree://") {
        errors.push(format!("{field} must start with enrtree://"));
    }
}

pub(super) fn validate_waku_direct_peer(
    field: &str,
    index: usize,
    peer: &WakuDirectPeerSetting,
    errors: &mut Vec<String>,
) {
    let peer_id = peer.peer_id.trim();
    let addr = peer.addr.trim();
    if peer_id.is_empty() {
        errors.push(format!("{field}[{index}].peer_id must not be empty"));
    } else if parse_peer_id(peer_id).is_err() {
        errors.push(format!(
            "{field}[{index}].peer_id must be a valid libp2p peer ID"
        ));
    }
    if addr.is_empty() {
        errors.push(format!("{field}[{index}].addr must not be empty"));
    } else if parse_multiaddr(addr).is_err() {
        errors.push(format!(
            "{field}[{index}].addr must be a valid libp2p multiaddr"
        ));
    }
}

pub(super) fn validate_optional_range(
    field: &str,
    value: Option<u64>,
    min: u64,
    max: u64,
    errors: &mut Vec<String>,
) {
    if let Some(value) = value {
        validate_range(field, value, min, max, errors);
    }
}

pub(super) fn validate_required_u64(field: &str, value: Option<u64>, errors: &mut Vec<String>) {
    if value.is_none() {
        errors.push(format!(
            "{field} is required when railgun_contract is custom"
        ));
    }
}

pub(super) fn validate_range(
    field: &str,
    value: u64,
    min: u64,
    max: u64,
    errors: &mut Vec<String>,
) {
    if value < min || value > max {
        errors.push(format!("{field} must be between {min} and {max}"));
    }
}

pub(super) fn parse_fixed_hex_32(
    value: &str,
) -> Result<alloy::primitives::FixedBytes<32>, hex::FromHexError> {
    hex::decode_to_array(value.strip_prefix("0x").unwrap_or(value)).map(Into::into)
}
