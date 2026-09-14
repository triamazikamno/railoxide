use thiserror::Error;

use super::types::HardwareAppVersion;

#[derive(Debug, Error)]
pub enum HardwareDerivationError {
    #[error(transparent)]
    RequestAuthority(#[from] crate::RpcBrokerError),
    #[error("invalid hardware derivation descriptor: {0}")]
    InvalidDescriptor(&'static str),
    #[error("invalid BIP32 derivation path segment: {0}")]
    InvalidPath(String),
    #[error("unsupported hardware KDF version: {0}")]
    UnsupportedKdfVersion(u16),
    #[error("hardware KDF expansion failed")]
    KdfExpand,
    #[error("hardware derivation client has no queued mock output")]
    MissingMockOutput,
    #[error("unexpected hardware response length: got {got}, expected {expected}")]
    UnexpectedResponseLength { got: usize, expected: usize },
    #[error("unexpected hardware response: {0}")]
    UnexpectedHardwareResponse(&'static str),
    #[error("unsupported Ledger Ethereum app version {actual}; requires {required} or newer")]
    UnsupportedLedgerEthereumAppVersion {
        actual: HardwareAppVersion,
        required: HardwareAppVersion,
    },
    #[cfg(feature = "hardware")]
    #[error("Ledger {operation} failed ({status:#06x}): {message}")]
    LedgerStatus {
        operation: &'static str,
        status: u16,
        message: &'static str,
    },
    #[cfg(feature = "hardware")]
    #[error("{0}")]
    LedgerUnavailable(&'static str),
    #[cfg(feature = "hardware")]
    #[error(transparent)]
    Ledger(#[from] coins_ledger::LedgerError),
    #[cfg(feature = "hardware")]
    #[error(transparent)]
    Trezor(#[from] trezor_client::Error),
    #[cfg(feature = "hardware")]
    #[error("Trezor Bridge transport failed: {0}")]
    TrezorBridge(String),
    #[cfg(feature = "hardware")]
    #[error("Trezor requested an app-entered passphrase but none was provided")]
    MissingTrezorAppPassphrase,
    #[cfg(feature = "hardware")]
    #[error("Trezor PIN matrix requests are not supported by this flow")]
    UnsupportedTrezorPinMatrix,
    #[cfg(feature = "hardware")]
    #[error("Trezor is locked")]
    TrezorLocked,
    #[cfg(feature = "hardware")]
    #[error("Trezor PIN entry was cancelled")]
    TrezorPinEntryCancelled,
}

impl HardwareDerivationError {
    #[must_use]
    #[cfg(not(feature = "hardware"))]
    pub const fn is_user_rejected(&self) -> bool {
        false
    }

    #[must_use]
    #[cfg(feature = "hardware")]
    pub fn is_user_rejected(&self) -> bool {
        match self {
            #[cfg(feature = "hardware")]
            Self::LedgerStatus { status, .. } => matches!(*status, 0x6982 | 0x6985),
            #[cfg(feature = "hardware")]
            Self::TrezorPinEntryCancelled => true,
            #[cfg(feature = "hardware")]
            Self::Trezor(trezor_client::Error::FailureResponse(failure)) => matches!(
                failure.code(),
                trezor_client::protos::failure::FailureType::Failure_ActionCancelled
                    | trezor_client::protos::failure::FailureType::Failure_PinCancelled
            ),
            _ => false,
        }
    }

    #[must_use]
    pub const fn is_early_device_readiness_error(&self) -> bool {
        match self {
            #[cfg(feature = "hardware")]
            Self::LedgerUnavailable(_) | Self::TrezorBridge(_) => true,
            #[cfg(feature = "hardware")]
            Self::TrezorLocked | Self::UnsupportedTrezorPinMatrix => true,
            #[cfg(feature = "hardware")]
            Self::LedgerStatus { status, .. } => {
                matches!(*status, 0x6511 | 0x6a15 | 0x6d00 | 0x6e00 | 0x6804 | 0x6b0c)
            }
            #[cfg(feature = "hardware")]
            Self::Trezor(trezor_client::Error::NoDeviceFound) => true,
            #[cfg(feature = "hardware")]
            Self::Trezor(trezor_client::Error::TransportConnect(
                trezor_client::transport::error::Error::DeviceNotFound,
            )) => true,
            _ => false,
        }
    }

    #[must_use]
    #[cfg(feature = "hardware")]
    pub const fn is_ledger_busy_error(&self) -> bool {
        matches!(
            self,
            Self::Ledger(coins_ledger::LedgerError::NativeTransportError(
                coins_ledger::transports::native::NativeTransportError::CantOpen(_),
            ))
        )
    }
}
