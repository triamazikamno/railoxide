//! Bitcoin, Solana and Tron: NEAR Intents destinations built into the wallet, outside the chain
//! registry, and their receiver address formats.
//!
//! Receivers are parsed with the networks' own codecs and kept in canonical form: a Bitcoin
//! segwit address lowercase, any other form as entered.

use alloy::primitives::Address;
use bech32::{hrp, segwit};
use serde::{Deserialize, Serialize};

/// A destination network without EVM addresses, chain configuration or RPC endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NonEvmNetwork {
    Bitcoin,
    Solana,
    Tron,
}

/// Why a receiver can't be used on a [`NonEvmNetwork`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NonEvmReceiverError {
    #[error("only mainnet addresses are supported")]
    Testnet,
    /// An EVM address or another network's address.
    #[error("the address must be a {} address", .0.display_name())]
    OtherNetwork(NonEvmNetwork),
    #[error("the address is invalid")]
    Invalid,
}

impl NonEvmNetwork {
    pub const ALL: [Self; 3] = [Self::Bitcoin, Self::Solana, Self::Tron];

    /// The network's 1Click `blockchain` name.
    #[must_use]
    pub const fn one_click_blockchain(self) -> &'static str {
        match self {
            Self::Bitcoin => "btc",
            Self::Solana => "sol",
            Self::Tron => "tron",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Bitcoin => "Bitcoin",
            Self::Solana => "Solana",
            Self::Tron => "Tron",
        }
    }

    #[must_use]
    pub const fn native_symbol(self) -> &'static str {
        match self {
            Self::Bitcoin => "BTC",
            Self::Solana => "SOL",
            Self::Tron => "TRX",
        }
    }

    /// The icon's embedded asset path, beside the chain icons of
    /// `railgun_ui::chain_icon_asset_path`.
    #[must_use]
    pub const fn icon_asset_path(self) -> &'static str {
        match self {
            Self::Bitcoin => "railgun-ui/chains/bitcoin.svg",
            Self::Solana => "railgun-ui/chains/solana.svg",
            Self::Tron => "railgun-ui/chains/tron.svg",
        }
    }

    /// `text` as a mainnet receiver on this network, in canonical form.
    pub fn parse_receiver(self, text: &str) -> Result<String, NonEvmReceiverError> {
        let text = text.trim();
        match self.parse_format(text) {
            Err(NonEvmReceiverError::Invalid)
                if text.parse::<Address>().is_ok()
                    || Self::ALL
                        .into_iter()
                        .any(|other| other != self && other.parse_format(text).is_ok()) =>
            {
                Err(NonEvmReceiverError::OtherNetwork(self))
            }
            parsed => parsed,
        }
    }

    /// The one network `text` is a receiver on, with the canonical receiver. The formats don't
    /// overlap, so `None` means `text` is a receiver on none of them.
    #[must_use]
    pub fn detect_receiver(text: &str) -> Option<(Self, String)> {
        let text = text.trim();
        let mut parsed = Self::ALL
            .into_iter()
            .filter_map(|network| Some((network, network.parse_format(text).ok()?)));
        let found = parsed.next()?;
        parsed.next().is_none().then_some(found)
    }

    /// A fresh receiver from the OS RNG, for one dry quote only: a P2WPKH address, 32 bytes on
    /// Solana, a Tron address. `None` when the RNG fails.
    pub(super) fn random_placeholder(self) -> Option<String> {
        match self {
            Self::Bitcoin => {
                let mut program = [0_u8; 20];
                getrandom::fill(&mut program).ok()?;
                segwit::encode_v0(hrp::BC, &program).ok()
            }
            Self::Solana => {
                let mut key = [0_u8; 32];
                getrandom::fill(&mut key).ok()?;
                Some(bs58::encode(key).into_string())
            }
            Self::Tron => {
                let mut address = [TRON_VERSION; 21];
                getrandom::fill(&mut address[1..]).ok()?;
                Some(bs58::encode(address).with_check().into_string())
            }
        }
    }

    /// `text`, already trimmed, in this network's format. Fails with `Testnet` or `Invalid`.
    fn parse_format(self, text: &str) -> Result<String, NonEvmReceiverError> {
        match self {
            Self::Bitcoin => parse_bitcoin(text),
            Self::Solana => match bs58::decode(text).into_vec() {
                // Off-curve keys are accepted: program-derived addresses can own tokens.
                Ok(key) if key.len() == 32 => Ok(text.to_owned()),
                _ => Err(NonEvmReceiverError::Invalid),
            },
            Self::Tron => match base58check(text) {
                Some(address) if address.len() == 21 && address[0] == TRON_VERSION => {
                    Ok(text.to_owned())
                }
                _ => Err(NonEvmReceiverError::Invalid),
            },
        }
    }
}

const TRON_VERSION: u8 = 0x41;

/// A segwit v0 or Taproot address, lowercased, or a base58check P2PKH or P2SH address as
/// entered. The segwit decoder also accepts future versions and v1 programs of other lengths,
/// which anyone can spend today, so those are rejected.
fn parse_bitcoin(text: &str) -> Result<String, NonEvmReceiverError> {
    if let Ok((prefix, version, program)) = segwit::decode(text) {
        return if prefix == hrp::TB || prefix == hrp::BCRT {
            Err(NonEvmReceiverError::Testnet)
        } else if prefix == hrp::BC
            && (version == segwit::VERSION_0
                || (version == segwit::VERSION_1 && program.len() == 32))
        {
            Ok(text.to_ascii_lowercase())
        } else {
            Err(NonEvmReceiverError::Invalid)
        };
    }
    match base58check(text).as_deref() {
        Some([0x00 | 0x05, payload @ ..]) if payload.len() == 20 => Ok(text.to_owned()),
        Some([0x6f | 0xc4, payload @ ..]) if payload.len() == 20 => {
            Err(NonEvmReceiverError::Testnet)
        }
        _ => Err(NonEvmReceiverError::Invalid),
    }
}

/// The version byte and payload of a base58check string.
fn base58check(text: &str) -> Option<Vec<u8>> {
    bs58::decode(text).with_check(None).into_vec().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAPROOT: &str = "bc1p5d7rjq7g6rdk2yhzks9smlaqtedr4dekq08ge8ztwac72sfr9rusxg3297";
    const P2WPKH: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    const P2SH: &str = "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy";
    const SOLANA: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const TRON: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
    const EVM: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn receivers_parse_to_canonical_mainnet_form() {
        use NonEvmNetwork::{Bitcoin, Solana, Tron};
        use NonEvmReceiverError::{Invalid, OtherNetwork, Testnet};

        let uppercase_p2wpkh = P2WPKH.to_ascii_uppercase();
        for (network, text, expected) in [
            (Bitcoin, TAPROOT, Ok(TAPROOT.to_owned())),
            (Bitcoin, uppercase_p2wpkh.as_str(), Ok(P2WPKH.to_owned())),
            (Bitcoin, P2SH, Ok(P2SH.to_owned())),
            (
                Bitcoin,
                "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx",
                Err(Testnet),
            ),
            // A v0 program with a bech32m checksum.
            (
                Bitcoin,
                "bc1qqypqxpq9qcrsszg2pvxq6rs0zqg3yyc5uyze8n",
                Err(Invalid),
            ),
            // A v2 program, and a v1 program of 20 bytes.
            (
                Bitcoin,
                "bc1zqypqxpq9qcrsszg2pvxq6rs0zqss2fw7",
                Err(Invalid),
            ),
            (
                Bitcoin,
                "bc1pqypqxpq9qcrsszg2pvxq6rs0zqg3yyc5h64j2c",
                Err(Invalid),
            ),
            (Solana, SOLANA, Ok(SOLANA.to_owned())),
            // 31 bytes.
            (
                Solana,
                "7DUeBUtEcb7nujVZRJmeBju3X1mo6PpnWNtJ9EBhdY",
                Err(Invalid),
            ),
            (Solana, TRON, Err(OtherNetwork(Solana))),
            (Tron, TRON, Ok(TRON.to_owned())),
            (
                Tron,
                "41a614f803b6fd780986a42c78ec9c7f77e6ded13c",
                Err(Invalid),
            ),
            // The last character changed, so the checksum doesn't match.
            (Tron, "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6u", Err(Invalid)),
        ] {
            assert_eq!(network.parse_receiver(text), expected, "{network:?} {text}");
        }
        for network in NonEvmNetwork::ALL {
            assert_eq!(network.parse_receiver(EVM), Err(OtherNetwork(network)));
        }

        // Base58check Bitcoin and Tron addresses have the same length, but not the same version.
        assert_eq!(
            NonEvmNetwork::detect_receiver(P2SH),
            Some((Bitcoin, P2SH.to_owned()))
        );
        assert_eq!(
            NonEvmNetwork::detect_receiver(TRON),
            Some((Tron, TRON.to_owned()))
        );
        assert_eq!(NonEvmNetwork::detect_receiver(EVM), None);
    }

    #[test]
    fn placeholders_are_fresh_receivers_on_their_network() {
        for network in NonEvmNetwork::ALL {
            let first = network.random_placeholder().unwrap();
            assert_eq!(network.parse_receiver(&first).as_ref(), Ok(&first));
            assert_ne!(network.random_placeholder().unwrap(), first, "{network:?}");
        }
    }
}
