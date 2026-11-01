//! Tron's mainnet address form: base58check of the version byte `0x41` and the 20 bytes the same
//! secp256k1 key has as an EVM address. `bridge::non_evm` parses Tron receivers with this rule.

use alloy::primitives::Address;

const TRON_ADDRESS_VERSION: u8 = 0x41;

/// The text isn't a mainnet base58check Tron address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a Tron address")]
pub struct InvalidTronAddress;

#[must_use]
pub fn encode_tron_address(address: Address) -> String {
    bs58::encode(tron_address_bytes(address))
        .with_check()
        .into_string()
}

/// `address` in Tron's 21-byte form: the version byte, then the 20 address bytes.
pub(super) fn tron_address_bytes(address: Address) -> [u8; 21] {
    let mut bytes = [TRON_ADDRESS_VERSION; 21];
    bytes[1..].copy_from_slice(address.as_slice());
    bytes
}

/// `text`, as given, as a mainnet base58check Tron address. EVM hex, Tron hex (`41…`) and other
/// networks' base58check addresses are refused.
pub fn parse_tron_address(text: &str) -> Result<Address, InvalidTronAddress> {
    match bs58::decode(text)
        .with_check(Some(TRON_ADDRESS_VERSION))
        .into_vec()
    {
        Ok(bytes) if bytes.len() == 21 => Ok(Address::from_slice(&bytes[1..])),
        _ => Err(InvalidTronAddress),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    #[test]
    fn addresses_round_trip_and_other_forms_are_refused() {
        let usdt = address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
        assert_eq!(
            encode_tron_address(usdt),
            "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t"
        );
        assert_eq!(parse_tron_address(&encode_tron_address(usdt)), Ok(usdt));
        let edge = Address::repeat_byte(0xff);
        assert_eq!(parse_tron_address(&encode_tron_address(edge)), Ok(edge));

        for text in [
            "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c",
            "41a614f803b6fd780986a42c78ec9c7f77e6ded13c",
            // Checksum mismatch: the last character changed.
            "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6u",
            // Bitcoin P2SH: base58check of the same length with version 0x05.
            "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy",
            // Surrounding whitespace is the caller's to trim.
            " TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t",
            "",
        ] {
            assert_eq!(parse_tron_address(text), Err(InvalidTronAddress), "{text}");
        }
    }
}
