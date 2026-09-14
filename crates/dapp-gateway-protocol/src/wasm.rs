//! Worker bindings. Errors are fixed strings and credential export is an explicit trust boundary.
use wasm_bindgen::prelude::*;
use zeroize::Zeroizing;

use crate::{Connection, HandshakeEvent, PairingCode, PeerId, ProtocolError, SessionSecret};

/// Worker-owned connection. It must never be handed to an untrusted content script or web page.
#[wasm_bindgen]
pub struct GatewayClient {
    connection: Connection,
    hello: Vec<u8>,
    event: u8,
}

#[wasm_bindgen]
impl GatewayClient {
    /// Constructs a pending pairing. The six ASCII digits are never included in errors.
    #[wasm_bindgen(js_name = pair)]
    pub fn pair(code: Vec<u8>) -> Result<Self, JsValue> {
        let code = Zeroizing::new(code);
        let code = PairingCode::new(
            code.as_slice()
                .try_into()
                .map_err(|_| failure(ProtocolError::InvalidMessage))?,
        )
        .map_err(failure)?;
        let (connection, hello) = Connection::client_pair(code);
        Ok(Self {
            connection,
            hello,
            event: 0,
        })
    }

    /// Imports only the dedicated transport PSK from trusted browser storage.
    #[wasm_bindgen(js_name = reconnect)]
    pub fn reconnect(peer_id: &[u8], secret: Vec<u8>) -> Result<Self, JsValue> {
        let secret = Zeroizing::new(secret);
        let peer_id = PeerId::from_bytes(
            peer_id
                .try_into()
                .map_err(|_| failure(ProtocolError::InvalidMessage))?,
        );
        let secret = SessionSecret::from_storage(
            secret
                .as_slice()
                .try_into()
                .map_err(|_| failure(ProtocolError::InvalidMessage))?,
        );
        let (connection, hello) = Connection::client_reconnect(peer_id, secret);
        Ok(Self {
            connection,
            hello,
            event: 0,
        })
    }

    #[wasm_bindgen(js_name = initialHello)]
    #[must_use]
    pub fn initial_hello(&self) -> Vec<u8> {
        self.hello.clone()
    }

    /// Returns the next wire message, or an empty array when the caller must handle an event.
    #[wasm_bindgen(js_name = receiveHandshake)]
    pub fn receive_handshake(&mut self, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.event = 0;
        let step = self.connection.receive_handshake(bytes).map_err(failure)?;
        self.event = match step.event {
            None => 0,
            Some(HandshakeEvent::CredentialReceived) => 1,
            Some(HandshakeEvent::PairingReadyToCommit) => 2,
            Some(HandshakeEvent::Authenticated) => 3,
        };
        Ok(step.outbound.unwrap_or_default())
    }

    /// 0 means no event, 1 requires storage acknowledgement, 3 means authenticated.
    #[wasm_bindgen(js_name = lastEvent)]
    #[must_use]
    #[expect(
        clippy::missing_const_for_fn,
        reason = "wasm-bindgen cannot export const functions"
    )]
    pub fn last_event(&self) -> u8 {
        self.event
    }

    #[wasm_bindgen(js_name = isAuthenticated)]
    #[must_use]
    #[expect(
        clippy::missing_const_for_fn,
        reason = "wasm-bindgen cannot export const functions"
    )]
    pub fn is_authenticated(&self) -> bool {
        self.connection.authenticated_peer_id().is_some()
    }

    /// Candidate peer identity for trusted credential storage, not wallet authorization.
    #[wasm_bindgen(js_name = pendingPeerId)]
    #[must_use]
    pub fn pending_peer_id(&self) -> Vec<u8> {
        self.connection.pending_peer_id().to_bytes().to_vec()
    }

    /// The returned bytes may only enter `TRUSTED_CONTEXTS` local credential storage.
    #[wasm_bindgen(js_name = exportPendingCredentialForStorage)]
    pub fn export_pending_credential_for_storage(&self) -> Result<Vec<u8>, JsValue> {
        let credential = self
            .connection
            .pending_credential()
            .ok_or_else(|| failure(ProtocolError::InvalidState))?;
        Ok(credential.export_for_storage().to_vec())
    }

    #[wasm_bindgen(js_name = acknowledgeCredential)]
    pub fn acknowledge_credential(&mut self) -> Result<Vec<u8>, JsValue> {
        self.event = 0;
        self.connection.acknowledge_credential().map_err(failure)
    }

    /// Returns ordered `Uint8Array` records; the owner sends every record sequentially.
    #[wasm_bindgen(js_name = sealMessage)]
    pub fn seal_message(&mut self, bytes: &[u8]) -> Result<js_sys::Array, JsValue> {
        let frames = self.connection.seal_message(bytes).map_err(failure)?;
        let output = js_sys::Array::new();
        for frame in frames {
            output.push(&js_sys::Uint8Array::from(frame.as_slice()));
        }
        Ok(output)
    }

    /// Returns undefined until a complete message is available. Time is monotonic milliseconds.
    #[wasm_bindgen(js_name = receiveFrame)]
    pub fn receive_frame(&mut self, bytes: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, JsValue> {
        self.connection
            .receive_frame(bytes, now_ms)
            .map_err(failure)
    }

    #[wasm_bindgen(js_name = expireAssembly)]
    pub fn expire_assembly(&mut self, now_ms: u64) -> Result<(), JsValue> {
        self.connection.expire_assembly(now_ms).map_err(failure)
    }

    pub fn close(&mut self) {
        self.event = 0;
        self.connection.close();
    }
}

fn failure(error: ProtocolError) -> JsValue {
    JsValue::from_str(match error {
        ProtocolError::IncompatibleVersion => "Incompatible gateway protocol version",
        _ => "Gateway connection failed",
    })
}
