use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

use super::error::HardwareDerivationError;
use super::path::{HARDENED_BIP32_INDEX, format_bip32_path, hardened_bip32_index};
use super::types::HardwareDeviceKind;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HardwarePublicAccountPathKind {
    LedgerLive,
    TrezorSuite,
    LedgerBip44,
    TrezorBip44,
}

impl HardwarePublicAccountPathKind {
    #[must_use]
    pub const fn device_kind(self) -> HardwareDeviceKind {
        match self {
            Self::LedgerLive | Self::LedgerBip44 => HardwareDeviceKind::Ledger,
            Self::TrezorSuite | Self::TrezorBip44 => HardwareDeviceKind::Trezor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwarePublicAccountDescriptor {
    pub device_kind: HardwareDeviceKind,
    pub path_kind: HardwarePublicAccountPathKind,
    pub path: Vec<u32>,
    #[serde(default)]
    pub wallet_account_index: u32,
    #[serde(default, alias = "account_index")]
    pub public_account_index: u32,
}

impl HardwarePublicAccountDescriptor {
    pub fn for_wallet_public_index(
        device_kind: HardwareDeviceKind,
        wallet_account_index: u32,
        public_account_index: u32,
    ) -> Result<Self, HardwareDerivationError> {
        if wallet_account_index >= HARDENED_BIP32_INDEX {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public wallet account index is too large",
            ));
        }
        if public_account_index >= HARDENED_BIP32_INDEX {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public account index is too large",
            ));
        }
        let descriptor = Self::for_wallet_public_index_unchecked(
            device_kind,
            wallet_account_index,
            public_account_index,
        );
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn for_device_index(
        device_kind: HardwareDeviceKind,
        account_index: u32,
    ) -> Result<Self, HardwareDerivationError> {
        Self::for_wallet_public_index(device_kind, 0, account_index)
    }

    pub fn validate(&self) -> Result<(), HardwareDerivationError> {
        if self.wallet_account_index >= HARDENED_BIP32_INDEX {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public wallet account index is too large",
            ));
        }
        if self.public_account_index >= HARDENED_BIP32_INDEX {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public account index is too large",
            ));
        }
        if self.path_kind.device_kind() != self.device_kind {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public account path kind does not match device kind",
            ));
        }
        if self.path.len() != 5 {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public account path must contain 5 segments",
            ));
        }
        let expected = Self::for_wallet_public_index_unchecked(
            self.device_kind,
            self.wallet_account_index,
            self.public_account_index,
        );
        let legacy =
            Self::legacy_for_device_index_unchecked(self.device_kind, self.public_account_index);
        if (self.path_kind != expected.path_kind || self.path != expected.path)
            && (self.path_kind != legacy.path_kind || self.path != legacy.path)
        {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "hardware public account path does not match account index",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn path_display(&self) -> String {
        format_bip32_path(&self.path)
    }

    fn for_wallet_public_index_unchecked(
        device_kind: HardwareDeviceKind,
        wallet_account_index: u32,
        public_account_index: u32,
    ) -> Self {
        match device_kind {
            HardwareDeviceKind::Ledger => Self {
                device_kind,
                path_kind: HardwarePublicAccountPathKind::LedgerBip44,
                path: vec![
                    hardened_bip32_index(44),
                    hardened_bip32_index(60),
                    hardened_bip32_index(wallet_account_index),
                    0,
                    public_account_index,
                ],
                wallet_account_index,
                public_account_index,
            },
            HardwareDeviceKind::Trezor => Self {
                device_kind,
                path_kind: HardwarePublicAccountPathKind::TrezorBip44,
                path: vec![
                    hardened_bip32_index(44),
                    hardened_bip32_index(60),
                    hardened_bip32_index(wallet_account_index),
                    0,
                    public_account_index,
                ],
                wallet_account_index,
                public_account_index,
            },
        }
    }

    fn legacy_for_device_index_unchecked(
        device_kind: HardwareDeviceKind,
        account_index: u32,
    ) -> Self {
        match device_kind {
            HardwareDeviceKind::Ledger => Self {
                device_kind,
                path_kind: HardwarePublicAccountPathKind::LedgerLive,
                path: vec![
                    hardened_bip32_index(44),
                    hardened_bip32_index(60),
                    hardened_bip32_index(account_index),
                    0,
                    0,
                ],
                wallet_account_index: 0,
                public_account_index: account_index,
            },
            HardwareDeviceKind::Trezor => Self {
                device_kind,
                path_kind: HardwarePublicAccountPathKind::TrezorSuite,
                path: vec![
                    hardened_bip32_index(44),
                    hardened_bip32_index(60),
                    hardened_bip32_index(0),
                    0,
                    account_index,
                ],
                wallet_account_index: 0,
                public_account_index: account_index,
            },
        }
    }
}

/// Public key and chain code at `m/44'/60'/{wallet}'/0`. Links every derived
/// address, so no `Debug`; memory only, never log.
pub struct HardwareAccountPublicKey {
    /// SEC1, compressed or uncompressed.
    pub public_key: Vec<u8>,
    pub chain_code: [u8; 32],
}

/// `m/44'/60'/{wallet}'/0`, the parent of every hardware public account path.
pub fn hardware_public_account_key_path(
    wallet_account_index: u32,
) -> Result<Vec<u32>, HardwareDerivationError> {
    if wallet_account_index >= HARDENED_BIP32_INDEX {
        return Err(HardwareDerivationError::InvalidDescriptor(
            "hardware public wallet account index is too large",
        ));
    }
    Ok(vec![
        hardened_bip32_index(44),
        hardened_bip32_index(60),
        hardened_bip32_index(wallet_account_index),
        0,
    ])
}

/// Uncompressed public key at `m/44'/60'/0'/0` for the `abandon ... about` test mnemonic.
#[cfg(test)]
pub(crate) const TEST_ACCOUNT_PUBLIC_KEY: [u8; 65] = [
    0x04, 0xcc, 0xf9, 0x61, 0x84, 0xb4, 0xd3, 0x42, 0xc5, 0x23, 0x93, 0x69, 0x10, 0xe0, 0x22, 0x2b,
    0xe7, 0x13, 0x16, 0x54, 0x84, 0x2d, 0xb7, 0x5b, 0xb1, 0xa5, 0xcb, 0xc7, 0x72, 0xfe, 0x21, 0xb2,
    0xd6, 0x28, 0x5b, 0x9b, 0xa7, 0x6d, 0x05, 0x47, 0x1e, 0x77, 0x1e, 0x3a, 0x3b, 0xe1, 0x8a, 0x31,
    0x3a, 0x6c, 0xe7, 0xa1, 0xd3, 0xb6, 0xa2, 0xd2, 0x1f, 0x5b, 0x60, 0xba, 0x8c, 0x03, 0x09, 0x73,
    0x38,
];

/// Chain code at `m/44'/60'/0'/0` for the `abandon ... about` test mnemonic.
#[cfg(test)]
pub(crate) const TEST_ACCOUNT_CHAIN_CODE: [u8; 32] = [
    0xc8, 0x79, 0xd1, 0x36, 0xf0, 0x20, 0x03, 0xdc, 0x80, 0x48, 0x11, 0xe4, 0x23, 0x90, 0x71, 0x4e,
    0xa0, 0x64, 0x48, 0xae, 0xae, 0xe1, 0x58, 0xc6, 0x2d, 0x65, 0xf1, 0x2a, 0x42, 0x1d, 0x98, 0x8f,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedHardwarePublicAccount {
    descriptor: HardwarePublicAccountDescriptor,
    address: Address,
}

impl ConfirmedHardwarePublicAccount {
    #[cfg(any(feature = "hardware", test, feature = "test-support"))]
    pub(super) const fn new(descriptor: HardwarePublicAccountDescriptor, address: Address) -> Self {
        Self {
            descriptor,
            address,
        }
    }

    #[must_use]
    pub const fn descriptor(&self) -> &HardwarePublicAccountDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn address(&self) -> Address {
        self.address
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn new_for_tests(
        descriptor: HardwarePublicAccountDescriptor,
        address: Address,
    ) -> Self {
        Self::new(descriptor, address)
    }
}
