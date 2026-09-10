//! Shared gateway cryptography and ordered records for native and browser workers.
//!
//! Owners enforce admission, attempt limits, deadlines, durable peer storage and revocation.
//! Authentication grants transport identity only, never wallet authority. Application envelopes
//! must carry request ownership. No decrypted application data is exposed before authentication.
//!
//! Secret wrappers and temporary plaintexts are zeroized. SPAKE2 and Snow retain internal secret
//! copies whose complete zeroization is not guaranteed by these dependencies.

mod handshake;
mod records;
#[cfg(target_family = "wasm")]
mod wasm;
#[cfg(target_family = "wasm")]
pub use wasm::GatewayClient;

pub use handshake::{ClientHello, Connection, HandshakeEvent, HandshakeStep, ServerAuth};
pub use records::{ASSEMBLY_TIMEOUT_MS, MAX_MESSAGE_LEN, MAX_RECORD_LEN};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Version supported by this release.
pub const PROTOCOL_VERSION: u16 = 1;
/// Maximum size of each handshake message, including encrypted confirmations.
pub const MAX_HANDSHAKE_LEN: usize = 1024;

/// Fixed, input-independent failures. Never attach dependency errors or payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    InvalidMessage,
    IncompatibleVersion,
    AuthenticationFailed,
    InvalidState,
    ResourceLimit,
    Expired,
    RandomnessUnavailable,
}

/// Opaque transport identity. It contains no wallet or browser-origin identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerId([u8; 16]);

impl PeerId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Six decimal digits. Owners enforce expiry, single use and bounded attempts.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PairingCode([u8; 6]);

impl PairingCode {
    pub fn new(bytes: [u8; 6]) -> Result<Self, ProtocolError> {
        let code = Self(bytes);
        if code.0.iter().all(u8::is_ascii_digit) {
            Ok(code)
        } else {
            Err(ProtocolError::InvalidMessage)
        }
    }

    /// Explicit disclosure boundary for the desktop pairing display only.
    #[must_use]
    pub const fn expose_for_display(&self) -> &[u8; 6] {
        &self.0
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode([REDACTED])")
    }
}

/// Random transport credential. Never derive this from wallet secrets or a pairing code.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionSecret([u8; 32]);

impl SessionSecret {
    pub fn random() -> Result<Self, ProtocolError> {
        let mut secret = Self([0; 32]);
        getrandom02::getrandom(&mut secret.0).map_err(|_| ProtocolError::RandomnessUnavailable)?;
        Ok(secret)
    }

    /// Imports the dedicated transport credential from trusted owner storage.
    #[must_use]
    pub const fn from_storage(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Explicit plaintext export for trusted transport-credential storage only.
    /// The caller owns and must protect or erase this copy.
    #[must_use]
    pub const fn export_for_storage(&self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionSecret([REDACTED])")
    }
}
