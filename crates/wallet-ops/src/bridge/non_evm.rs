//! Bitcoin, Solana, Tron and Zcash: NEAR Intents destinations built into the wallet, outside the
//! chain registry, and their receiver address formats.
//!
//! Receivers are parsed with the networks' own codecs and kept in canonical form: a Bitcoin
//! segwit address and a Zcash TEX or unified address lowercase, any other form as entered.

use alloy::primitives::Address;
use bech32::{hrp, segwit};
use serde::{Deserialize, Serialize};
use zcash_address::{
    ConversionError, ToAddress, TryFromAddress, ZcashAddress,
    unified::{self, Container as _},
};
use zcash_protocol::{PoolType, consensus::NetworkType};

use crate::tron::address::{encode_tron_address, parse_tron_address};

/// A destination network without EVM addresses, chain configuration or RPC endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NonEvmNetwork {
    Bitcoin,
    Solana,
    Tron,
    Zcash,
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
    /// A Zcash Sapling address, or a unified address with no Orchard or transparent receiver.
    #[error("Sapling addresses are not supported")]
    Sapling,
}

impl NonEvmNetwork {
    pub const ALL: [Self; 4] = [Self::Bitcoin, Self::Solana, Self::Tron, Self::Zcash];

    /// The network's 1Click `blockchain` name.
    #[must_use]
    pub const fn one_click_blockchain(self) -> &'static str {
        match self {
            Self::Bitcoin => "btc",
            Self::Solana => "sol",
            Self::Tron => "tron",
            Self::Zcash => "zec",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Bitcoin => "Bitcoin",
            Self::Solana => "Solana",
            Self::Tron => railgun_ui::TRON_NAME,
            Self::Zcash => "Zcash",
        }
    }

    #[must_use]
    pub const fn native_symbol(self) -> &'static str {
        match self {
            Self::Bitcoin => "BTC",
            Self::Solana => "SOL",
            Self::Tron => "TRX",
            Self::Zcash => "ZEC",
        }
    }

    /// The icon's embedded asset path, beside the chain icons of
    /// `railgun_ui::chain_icon_asset_path`.
    #[must_use]
    pub const fn icon_asset_path(self) -> &'static str {
        match self {
            Self::Bitcoin => "railgun-ui/chains/bitcoin.svg",
            Self::Solana => "railgun-ui/chains/solana.svg",
            Self::Tron => railgun_ui::TRON_ICON_ASSET_PATH,
            Self::Zcash => "railgun-ui/chains/zcash.svg",
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
    /// Solana, a Tron address, a Zcash `t1` address. `None` when the RNG fails.
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
                let mut address = [0_u8; 20];
                getrandom::fill(&mut address).ok()?;
                Some(encode_tron_address(Address::from(address)))
            }
            Self::Zcash => {
                let mut key_hash = [0_u8; 20];
                getrandom::fill(&mut key_hash).ok()?;
                Some(ZcashAddress::from_transparent_p2pkh(NetworkType::Main, key_hash).encode())
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
            Self::Tron => parse_tron_address(text)
                .map(|_| text.to_owned())
                .map_err(|_| NonEvmReceiverError::Invalid),
            Self::Zcash => parse_zcash(text),
        }
    }
}

/// A Zcash receiver kind, as far as accepting it as a swap receiver goes.
enum ZcashKind {
    /// `t1`, `t3`, `tex1`, or `u1` with an Orchard or transparent receiver.
    Accepted,
    /// `zs1`, or `u1` with only a Sapling receiver.
    Sapling,
}

/// Sprout addresses and Revision 2 unified addresses (`zu1`, `tu1`) fail to convert. 1Click
/// hasn't been seen to accept the latter, and they can carry an expiry.
impl TryFromAddress for ZcashKind {
    type Error = ();

    fn try_from_sapling(_: NetworkType, _: [u8; 43]) -> Result<Self, ConversionError<()>> {
        Ok(Self::Sapling)
    }

    fn try_from_unified(
        _: NetworkType,
        address: unified::Address,
    ) -> Result<Self, ConversionError<()>> {
        if address.revision() != unified::Revision::R0 {
            Err(ConversionError::User(()))
        } else if address.has_receiver_of_type(PoolType::ORCHARD)
            || address.has_receiver_of_type(PoolType::TRANSPARENT)
        {
            Ok(Self::Accepted)
        } else {
            Ok(Self::Sapling)
        }
    }

    fn try_from_transparent_p2pkh(
        _: NetworkType,
        _: [u8; 20],
    ) -> Result<Self, ConversionError<()>> {
        Ok(Self::Accepted)
    }

    fn try_from_transparent_p2sh(_: NetworkType, _: [u8; 20]) -> Result<Self, ConversionError<()>> {
        Ok(Self::Accepted)
    }

    fn try_from_tex(_: NetworkType, _: [u8; 20]) -> Result<Self, ConversionError<()>> {
        Ok(Self::Accepted)
    }
}

/// A mainnet `t1`, `t3` or `tex1` address, or a `u1` address with an Orchard or transparent
/// receiver. The NEAR Zcash connector pays only those receivers, and may pay a unified address
/// that has both through its transparent receiver.
fn parse_zcash(text: &str) -> Result<String, NonEvmReceiverError> {
    // `zcash_address` decodes bech32 in lowercase only, but it may also be written in uppercase.
    // Base58check forms all contain lowercase letters, so they stay as entered.
    let text = if text.bytes().any(|byte| byte.is_ascii_lowercase()) {
        text.to_owned()
    } else {
        text.to_ascii_lowercase()
    };
    let (network, kind) = ZcashAddress::try_from_encoded(&text)
        .ok()
        .and_then(|address| address.convert::<(NetworkType, ZcashKind)>().ok())
        .ok_or(NonEvmReceiverError::Invalid)?;
    if network != NetworkType::Main {
        return Err(NonEvmReceiverError::Testnet);
    }
    match kind {
        ZcashKind::Accepted => Ok(text),
        ZcashKind::Sapling => Err(NonEvmReceiverError::Sapling),
    }
}

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
    const SAPLING: &str =
        "zs1z7rejlpsa98s2rrrfkwmaxu53e4ue0ulcrw0h4x5g8jl04tak0d3mm47vdtahatqrlkngh9slya";

    fn zcash_unified(
        revision: unified::Revision,
        receivers: impl IntoIterator<Item = unified::Receiver>,
        network: NetworkType,
    ) -> String {
        use zcash_address::unified::Encoding as _;
        let items = receivers.into_iter().map(unified::Uitem::Data).collect();
        unified::Address::try_from_items(revision, items)
            .unwrap()
            .encode(&network)
    }

    #[test]
    fn receivers_parse_to_canonical_mainnet_form() {
        use NonEvmNetwork::{Bitcoin, Solana, Tron, Zcash};
        use NonEvmReceiverError::{Invalid, OtherNetwork, Sapling, Testnet};
        use unified::{Receiver, Revision};

        let main = NetworkType::Main;
        let t1 = ZcashAddress::from_transparent_p2pkh(main, [1; 20]).encode();
        let t3 = ZcashAddress::from_transparent_p2sh(main, [2; 20]).encode();
        let tex = ZcashAddress::from_tex(main, [3; 20]).encode();
        let testnet_t = ZcashAddress::from_transparent_p2pkh(NetworkType::Test, [1; 20]).encode();
        let orchard = [Receiver::Orchard([4; 43]), Receiver::P2pkh([1; 20])];
        let u1 = zcash_unified(Revision::R0, orchard.clone(), main);
        let uppercase_u1 = u1.to_ascii_uppercase();
        let sapling_p2sh = zcash_unified(
            Revision::R0,
            [Receiver::Sapling([5; 43]), Receiver::P2sh([2; 20])],
            main,
        );
        let sapling_only = zcash_unified(Revision::R0, [Receiver::Sapling([5; 43])], main);
        let testnet_u = zcash_unified(Revision::R0, orchard.clone(), NetworkType::Test);
        let revision_2 = zcash_unified(Revision::R2, orchard, main);
        assert!(t1.starts_with("t1") && t3.starts_with("t3") && tex.starts_with("tex1"));
        assert!(u1.starts_with("u1") && revision_2.starts_with("tu1"));

        let uppercase_p2wpkh = P2WPKH.to_ascii_uppercase();
        for (network, text, expected) in [
            (Zcash, u1.as_str(), Ok(u1.clone())),
            (Zcash, uppercase_u1.as_str(), Ok(u1.clone())),
            (Zcash, t1.as_str(), Ok(t1.clone())),
            (Zcash, t3.as_str(), Ok(t3.clone())),
            (Zcash, tex.as_str(), Ok(tex.clone())),
            (Zcash, sapling_p2sh.as_str(), Ok(sapling_p2sh.clone())),
            (Zcash, sapling_only.as_str(), Err(Sapling)),
            (Zcash, SAPLING, Err(Sapling)),
            (Zcash, testnet_t.as_str(), Err(Testnet)),
            (Zcash, testnet_u.as_str(), Err(Testnet)),
            (Zcash, revision_2.as_str(), Err(Invalid)),
            (Zcash, P2SH, Err(OtherNetwork(Zcash))),
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
        // Zcash base58check payloads have a two-byte version, and its bech32 prefixes are its own.
        for zcash in [&t1, &t3, &tex, &u1] {
            assert_eq!(
                NonEvmNetwork::detect_receiver(zcash),
                Some((Zcash, zcash.clone()))
            );
        }
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
