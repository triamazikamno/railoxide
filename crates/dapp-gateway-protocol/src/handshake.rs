use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::{
    MAX_HANDSHAKE_LEN, PROTOCOL_VERSION, PairingCode, PeerId, ProtocolError, SessionSecret,
    records::Records,
};

const PAKE_LEN: usize = 33;
const HELLO_LEN: usize = 22;
const SERVER_HELLO_LEN: usize = 52;
const CLIENT_CONFIRM: &[u8] = b"gateway/v1/client-confirm";
const SERVER_CONFIRM: &[u8] = b"gateway/v1/server-confirm";
const CONFIRMED: &[u8] = b"gateway/v1/confirmed";
const CREDENTIAL_ACK: &[u8] = b"gateway/v1/credential-stored";
const READY: &[u8] = b"gateway/v1/committed";

/// Unauthenticated admission metadata. Decoding this never authenticates a peer.
#[derive(Clone, Copy, Debug)]
pub struct ClientHello {
    minimum: u16,
    maximum: u16,
    pairing: bool,
    peer_id: PeerId,
}

impl ClientHello {
    #[must_use]
    pub const fn pair() -> Self {
        Self {
            minimum: PROTOCOL_VERSION,
            maximum: PROTOCOL_VERSION,
            pairing: true,
            peer_id: PeerId::from_bytes([0; 16]),
        }
    }

    #[must_use]
    pub const fn reconnect(peer_id: PeerId) -> Self {
        Self {
            peer_id,
            pairing: false,
            ..Self::pair()
        }
    }

    #[must_use]
    pub const fn is_pairing(self) -> bool {
        self.pairing
    }

    /// Claimed identity for reconnect lookup; trustworthy only after authentication.
    #[must_use]
    pub const fn claimed_peer_id(self) -> PeerId {
        self.peer_id
    }

    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend_from_slice(&self.minimum.to_be_bytes());
        bytes.extend_from_slice(&self.maximum.to_be_bytes());
        bytes.push(u8::from(self.pairing));
        bytes.extend_from_slice(&self.peer_id.to_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() != HELLO_LEN || bytes[0] != 1 || bytes[5] > 1 {
            return Err(ProtocolError::InvalidMessage);
        }
        let minimum = u16::from_be_bytes([bytes[1], bytes[2]]);
        let maximum = u16::from_be_bytes([bytes[3], bytes[4]]);
        if minimum > PROTOCOL_VERSION || maximum < PROTOCOL_VERSION || minimum > maximum {
            return Err(ProtocolError::IncompatibleVersion);
        }
        let hello = Self {
            minimum,
            maximum,
            pairing: bytes[5] == 1,
            peer_id: PeerId::from_bytes(
                bytes[6..]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidMessage)?,
            ),
        };
        if hello.pairing && hello.peer_id.to_bytes() != [0; 16] {
            return Err(ProtocolError::InvalidMessage);
        }
        Ok(hello)
    }
}

/// Selected by the native owner after atomic admission and credential lookup.
pub enum ServerAuth {
    Pair { code: PairingCode, peer_id: PeerId },
    Reconnect(SessionSecret),
}

/// Persistence barriers and final transport authentication, never wallet authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeEvent {
    CredentialReceived,
    PairingReadyToCommit,
    Authenticated,
}

/// Send the outbound message before processing subsequent inbound messages.
pub struct HandshakeStep {
    pub outbound: Option<Vec<u8>>,
    pub event: Option<HandshakeEvent>,
}

impl HandshakeStep {
    const fn output(outbound: Vec<u8>) -> Self {
        Self {
            outbound: Some(outbound),
            event: None,
        }
    }
    const fn event(event: HandshakeEvent) -> Self {
        Self {
            outbound: None,
            event: Some(event),
        }
    }
}

enum Phase {
    ClientHello(ServerAuth),
    ServerPake {
        pake: Box<Spake2<Ed25519Group>>,
        context: Vec<u8>,
    },
    ServerNoise(Box<snow::HandshakeState>),
    ClientNoise(Box<snow::HandshakeState>),
    ServerConfirm,
    ClientConfirm,
    ServerConfirmed,
    ClientCredential,
    ClientPersist,
    ServerCredentialAck,
    ServerCommit,
    ClientReady,
    Ready,
    Failed,
}

/// One connection, with fresh PAKE/Noise state and nonce counters. Any input failure is terminal.
/// Dropping this object is the owner's cancellation and handshake-timeout mechanism.
pub struct Connection {
    hello: ClientHello,
    peer_id: PeerId,
    phase: Phase,
    records: Option<Records>,
    credential: Option<SessionSecret>,
}

impl Connection {
    #[must_use]
    pub fn client_pair(code: PairingCode) -> (Self, Vec<u8>) {
        let hello = ClientHello::pair();
        (
            Self::new(
                hello,
                Phase::ClientHello(ServerAuth::Pair {
                    code,
                    peer_id: hello.peer_id,
                }),
            ),
            hello.encode(),
        )
    }

    #[must_use]
    pub fn client_reconnect(peer_id: PeerId, secret: SessionSecret) -> (Self, Vec<u8>) {
        let hello = ClientHello::reconnect(peer_id);
        (
            Self::new(hello, Phase::ClientHello(ServerAuth::Reconnect(secret))),
            hello.encode(),
        )
    }

    pub fn server(hello: ClientHello, auth: ServerAuth) -> Result<(Self, Vec<u8>), ProtocolError> {
        let peer_id = match &auth {
            ServerAuth::Pair { peer_id, .. } if hello.pairing => *peer_id,
            ServerAuth::Reconnect(_) if !hello.pairing => hello.peer_id,
            _ => return Err(ProtocolError::InvalidMessage),
        };
        let mut challenge = [0; 32];
        getrandom02::getrandom(&mut challenge).map_err(|_| ProtocolError::RandomnessUnavailable)?;
        let mut reply = vec![2];
        reply.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        reply.push(u8::from(hello.pairing));
        reply.extend_from_slice(&peer_id.to_bytes());
        reply.extend_from_slice(&challenge);
        let context = transcript(hello, &reply);
        let phase = match auth {
            ServerAuth::Pair { code, .. } => {
                let (a, b) = identities(&context);
                let (pake, message) =
                    Spake2::<Ed25519Group>::start_b(&Password::new(&code.0), &a, &b);
                reply.extend_from_slice(&message);
                Phase::ServerPake {
                    pake: Box::new(pake),
                    context,
                }
            }
            ServerAuth::Reconnect(secret) => {
                Phase::ServerNoise(Box::new(noise(&secret.0, &context, false)?))
            }
        };
        let mut connection = Self::new(hello, phase);
        connection.peer_id = peer_id;
        Ok((connection, reply))
    }

    const fn new(hello: ClientHello, phase: Phase) -> Self {
        Self {
            hello,
            peer_id: hello.peer_id,
            phase,
            records: None,
            credential: None,
        }
    }

    /// Authenticated identity is available only after the final persistence barrier.
    #[must_use]
    pub const fn authenticated_peer_id(&self) -> Option<PeerId> {
        if matches!(self.phase, Phase::Ready) {
            Some(self.peer_id)
        } else {
            None
        }
    }

    /// Candidate identity for credential persistence, not request admission.
    #[must_use]
    pub const fn pending_peer_id(&self) -> PeerId {
        self.peer_id
    }

    #[must_use]
    pub const fn pending_credential(&self) -> Option<&SessionSecret> {
        self.credential.as_ref()
    }

    pub fn receive_handshake(&mut self, bytes: &[u8]) -> Result<HandshakeStep, ProtocolError> {
        let result = if bytes.len() > MAX_HANDSHAKE_LEN {
            Err(ProtocolError::ResourceLimit)
        } else {
            self.advance(bytes)
        };
        if result.is_err() {
            self.close();
        }
        result
    }

    fn advance(&mut self, bytes: &[u8]) -> Result<HandshakeStep, ProtocolError> {
        let phase = std::mem::replace(&mut self.phase, Phase::Failed);
        match phase {
            Phase::ClientHello(auth) => self.client_start(bytes, auth),
            Phase::ServerPake { pake, context } => {
                if bytes.len() <= PAKE_LEN {
                    return Err(ProtocolError::InvalidMessage);
                }
                let key = Zeroizing::new(
                    pake.finish(&bytes[..PAKE_LEN])
                        .map_err(|_| ProtocolError::AuthenticationFailed)?,
                );
                let handshake = noise(&key, &context, false)?;
                self.server_noise(handshake, &bytes[PAKE_LEN..])
            }
            Phase::ServerNoise(handshake) => self.server_noise(*handshake, bytes),
            Phase::ClientNoise(mut handshake) => {
                read_noise(&mut handshake, bytes)?;
                self.records = Some(Records::new(
                    handshake
                        .into_transport_mode()
                        .map_err(|_| ProtocolError::AuthenticationFailed)?,
                ));
                self.phase = Phase::ClientConfirm;
                Ok(HandshakeStep::output(self.encrypt_control(CLIENT_CONFIRM)?))
            }
            Phase::ServerConfirm => {
                self.expect_control(bytes, CLIENT_CONFIRM)?;
                self.phase = Phase::ServerConfirmed;
                Ok(HandshakeStep::output(self.encrypt_control(SERVER_CONFIRM)?))
            }
            Phase::ClientConfirm => {
                self.expect_control(bytes, SERVER_CONFIRM)?;
                self.phase = if self.hello.pairing {
                    Phase::ClientCredential
                } else {
                    Phase::ClientReady
                };
                Ok(HandshakeStep::output(self.encrypt_control(CONFIRMED)?))
            }
            Phase::ServerConfirmed => {
                self.expect_control(bytes, CONFIRMED)?;
                if self.hello.pairing {
                    let credential = SessionSecret::random()?;
                    let mut payload = Zeroizing::new(vec![3]);
                    payload.extend_from_slice(&credential.0);
                    let outbound = self.encrypt_control(&payload)?;
                    self.credential = Some(credential);
                    self.phase = Phase::ServerCredentialAck;
                    Ok(HandshakeStep::output(outbound))
                } else {
                    self.finish()
                }
            }
            Phase::ClientCredential => {
                let payload = self.decrypt_control(bytes)?;
                if payload.len() != 33 || payload[0] != 3 {
                    return Err(ProtocolError::InvalidMessage);
                }
                self.credential = Some(SessionSecret::from_storage(
                    payload[1..]
                        .try_into()
                        .map_err(|_| ProtocolError::InvalidMessage)?,
                ));
                self.phase = Phase::ClientPersist;
                Ok(HandshakeStep::event(HandshakeEvent::CredentialReceived))
            }
            Phase::ServerCredentialAck => {
                self.expect_control(bytes, CREDENTIAL_ACK)?;
                self.phase = Phase::ServerCommit;
                Ok(HandshakeStep::event(HandshakeEvent::PairingReadyToCommit))
            }
            Phase::ClientReady => {
                self.expect_control(bytes, READY)?;
                self.phase = Phase::Ready;
                self.credential = None;
                Ok(HandshakeStep::event(HandshakeEvent::Authenticated))
            }
            _ => Err(ProtocolError::InvalidState),
        }
    }

    fn client_start(
        &mut self,
        bytes: &[u8],
        auth: ServerAuth,
    ) -> Result<HandshakeStep, ProtocolError> {
        let expected = SERVER_HELLO_LEN + if self.hello.pairing { PAKE_LEN } else { 0 };
        if bytes.len() != expected || bytes[0] != 2 || bytes[3] != u8::from(self.hello.pairing) {
            return Err(ProtocolError::InvalidMessage);
        }
        if u16::from_be_bytes([bytes[1], bytes[2]]) != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        let peer_id = PeerId::from_bytes(
            bytes[4..20]
                .try_into()
                .map_err(|_| ProtocolError::InvalidMessage)?,
        );
        if !self.hello.pairing && peer_id != self.peer_id {
            return Err(ProtocolError::AuthenticationFailed);
        }
        self.peer_id = peer_id;
        let context = transcript(self.hello, &bytes[..SERVER_HELLO_LEN]);
        let (mut handshake, mut outbound) = match auth {
            ServerAuth::Pair { code, .. } => {
                let (a, b) = identities(&context);
                let (pake, outbound) =
                    Spake2::<Ed25519Group>::start_a(&Password::new(&code.0), &a, &b);
                let key = Zeroizing::new(
                    pake.finish(&bytes[SERVER_HELLO_LEN..])
                        .map_err(|_| ProtocolError::AuthenticationFailed)?,
                );
                (noise(&key, &context, true)?, outbound)
            }
            ServerAuth::Reconnect(secret) => (noise(&secret.0, &context, true)?, Vec::new()),
        };
        outbound.extend_from_slice(&write_noise(&mut handshake)?);
        self.phase = Phase::ClientNoise(Box::new(handshake));
        Ok(HandshakeStep::output(outbound))
    }

    fn server_noise(
        &mut self,
        mut handshake: snow::HandshakeState,
        bytes: &[u8],
    ) -> Result<HandshakeStep, ProtocolError> {
        read_noise(&mut handshake, bytes)?;
        let outbound = write_noise(&mut handshake)?;
        self.records = Some(Records::new(
            handshake
                .into_transport_mode()
                .map_err(|_| ProtocolError::AuthenticationFailed)?,
        ));
        self.phase = Phase::ServerConfirm;
        Ok(HandshakeStep::output(outbound))
    }

    /// Call only after the browser has stored the pending credential in trusted local storage.
    pub fn acknowledge_credential(&mut self) -> Result<Vec<u8>, ProtocolError> {
        if !matches!(self.phase, Phase::ClientPersist) {
            self.close();
            return Err(ProtocolError::InvalidState);
        }
        self.phase = Phase::ClientReady;
        let result = self.encrypt_control(CREDENTIAL_ACK);
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Call only after durable desktop persistence and final code-expiry/revocation admission.
    pub fn commit_pairing(&mut self) -> Result<HandshakeStep, ProtocolError> {
        if !matches!(self.phase, Phase::ServerCommit) {
            self.close();
            return Err(ProtocolError::InvalidState);
        }
        let result = self.finish();
        if result.is_err() {
            self.close();
        }
        result
    }

    fn finish(&mut self) -> Result<HandshakeStep, ProtocolError> {
        let outbound = self.encrypt_control(READY)?;
        self.phase = Phase::Ready;
        self.credential = None;
        Ok(HandshakeStep {
            outbound: Some(outbound),
            event: Some(HandshakeEvent::Authenticated),
        })
    }

    fn encrypt_control(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        self.records
            .as_mut()
            .ok_or(ProtocolError::InvalidState)?
            .encrypt(bytes)
    }

    fn decrypt_control(&mut self, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, ProtocolError> {
        self.records
            .as_mut()
            .ok_or(ProtocolError::InvalidState)?
            .decrypt(bytes)
    }

    fn expect_control(&mut self, bytes: &[u8], expected: &[u8]) -> Result<(), ProtocolError> {
        if self.decrypt_control(bytes)?.as_slice() == expected {
            Ok(())
        } else {
            Err(ProtocolError::AuthenticationFailed)
        }
    }

    /// Encodes one complete application envelope, including the caller's request ownership.
    /// Send all returned records in order without interleaving another message.
    pub fn seal_message(&mut self, message: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let result = self
            .authorized_records()
            .and_then(|records| records.seal(message));
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Returns only complete envelopes. `now_ms` must be an owner-provided monotonic clock.
    pub fn receive_frame(
        &mut self,
        frame: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        let result = self
            .authorized_records()
            .and_then(|records| records.receive(frame, now_ms));
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Owners call this on their timer even when no further records arrive.
    pub fn expire_assembly(&mut self, now_ms: u64) -> Result<(), ProtocolError> {
        let result = self
            .authorized_records()
            .and_then(|records| records.expire(now_ms));
        if result.is_err() {
            self.close();
        }
        result
    }

    fn authorized_records(&mut self) -> Result<&mut Records, ProtocolError> {
        if !matches!(self.phase, Phase::Ready) {
            return Err(ProtocolError::InvalidState);
        }
        self.records.as_mut().ok_or(ProtocolError::InvalidState)
    }

    /// Cancels handshakes, destroys transport state and discards incomplete plaintext.
    pub fn close(&mut self) {
        self.phase = Phase::Failed;
        self.records = None;
        self.credential = None;
    }
}

fn transcript(hello: ClientHello, server_hello: &[u8]) -> Vec<u8> {
    let mut context = Vec::new();
    for field in [
        b"railoxide/dapp-gateway".as_slice(),
        &hello.encode(),
        server_hello,
    ] {
        context.extend_from_slice(&(field.len() as u32).to_be_bytes());
        context.extend_from_slice(field);
    }
    context
}

fn identities(context: &[u8]) -> (Identity, Identity) {
    let mut a = context.to_vec();
    a.extend_from_slice(b"\0browser/A");
    let mut b = context.to_vec();
    b.extend_from_slice(b"\0desktop/B");
    (Identity::new(&a), Identity::new(&b))
}

fn noise(
    key: &[u8],
    context: &[u8],
    initiator: bool,
) -> Result<snow::HandshakeState, ProtocolError> {
    if key.len() != 32 {
        return Err(ProtocolError::AuthenticationFailed);
    }
    let params = "Noise_NNpsk0_25519_ChaChaPoly_SHA256"
        .parse()
        .map_err(|_| ProtocolError::AuthenticationFailed)?;
    let builder = snow::Builder::new(params).prologue(context).psk(0, key);
    if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(|_| ProtocolError::AuthenticationFailed)
}

fn write_noise(handshake: &mut snow::HandshakeState) -> Result<Vec<u8>, ProtocolError> {
    let mut output = vec![0; MAX_HANDSHAKE_LEN];
    let len = handshake
        .write_message(&[], &mut output)
        .map_err(|_| ProtocolError::AuthenticationFailed)?;
    output.truncate(len);
    Ok(output)
}

fn read_noise(handshake: &mut snow::HandshakeState, bytes: &[u8]) -> Result<(), ProtocolError> {
    let mut plaintext = Zeroizing::new(vec![0; MAX_HANDSHAKE_LEN]);
    let len = handshake
        .read_message(bytes, &mut plaintext)
        .map_err(|_| ProtocolError::AuthenticationFailed)?;
    if len != 0 {
        return Err(ProtocolError::InvalidMessage);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ASSEMBLY_TIMEOUT_MS, MAX_MESSAGE_LEN, MAX_RECORD_LEN};

    fn code(value: [u8; 6]) -> PairingCode {
        PairingCode::new(value).expect("synthetic code")
    }
    fn output(step: HandshakeStep) -> Vec<u8> {
        step.outbound.expect("outbound flight")
    }

    fn confirmed_pair() -> (Connection, Connection) {
        let (mut client, hello) = Connection::client_pair(code(*b"000042"));
        let (mut server, reply) = Connection::server(
            ClientHello::decode(&hello).expect("hello"),
            ServerAuth::Pair {
                code: code(*b"000042"),
                peer_id: PeerId::from_bytes([7; 16]),
            },
        )
        .expect("server");
        let first = output(
            client
                .receive_handshake(&reply)
                .expect("PAKE and first Noise"),
        );
        let second = output(server.receive_handshake(&first).expect("second Noise"));
        assert!(server.authenticated_peer_id().is_none());
        let confirm = output(
            client
                .receive_handshake(&second)
                .expect("client confirmation"),
        );
        assert!(client.authenticated_peer_id().is_none());
        let reply = output(
            server
                .receive_handshake(&confirm)
                .expect("server confirmation"),
        );
        let confirmed = output(
            client
                .receive_handshake(&reply)
                .expect("reciprocal confirmation"),
        );
        let credential = output(
            server
                .receive_handshake(&confirmed)
                .expect("provision credential"),
        );
        assert_eq!(
            client
                .receive_handshake(&credential)
                .expect("credential")
                .event,
            Some(HandshakeEvent::CredentialReceived)
        );
        (client, server)
    }

    fn reconnect(secret: [u8; 32]) -> (Connection, Connection) {
        let peer = PeerId::from_bytes([7; 16]);
        let (mut client, hello) =
            Connection::client_reconnect(peer, SessionSecret::from_storage(secret));
        let (mut server, reply) = Connection::server(
            ClientHello::decode(&hello).expect("hello"),
            ServerAuth::Reconnect(SessionSecret::from_storage(secret)),
        )
        .expect("server");
        let first = output(client.receive_handshake(&reply).expect("first"));
        let second = output(server.receive_handshake(&first).expect("second"));
        assert!(server.authenticated_peer_id().is_none());
        let confirm = output(client.receive_handshake(&second).expect("confirm"));
        let reply = output(server.receive_handshake(&confirm).expect("reply"));
        let confirmed = output(client.receive_handshake(&reply).expect("confirmed"));
        let ready = server.receive_handshake(&confirmed).expect("server ready");
        assert_eq!(ready.event, Some(HandshakeEvent::Authenticated));
        assert_eq!(
            client
                .receive_handshake(&output(ready))
                .expect("client ready")
                .event,
            Some(HandshakeEvent::Authenticated)
        );
        (client, server)
    }

    #[test]
    fn pairing_requires_storage_ack_and_desktop_commit() {
        let (mut client, mut server) = confirmed_pair();
        assert!(client.authenticated_peer_id().is_none());
        assert!(server.authenticated_peer_id().is_none());
        assert_eq!(client.pending_peer_id(), server.pending_peer_id());
        assert_eq!(
            client
                .pending_credential()
                .expect("client pending")
                .export_for_storage(),
            server
                .pending_credential()
                .expect("server pending")
                .export_for_storage()
        );
        let saved = client
            .pending_credential()
            .expect("pending")
            .export_for_storage();
        let ack = client.acknowledge_credential().expect("browser persisted");
        assert_eq!(
            server.receive_handshake(&ack).expect("ack").event,
            Some(HandshakeEvent::PairingReadyToCommit)
        );
        assert!(server.authenticated_peer_id().is_none());
        let ready = server.commit_pairing().expect("desktop persisted");
        assert_eq!(ready.event, Some(HandshakeEvent::Authenticated));
        assert_eq!(
            client
                .receive_handshake(&output(ready))
                .expect("committed")
                .event,
            Some(HandshakeEvent::Authenticated)
        );
        assert_eq!(
            client.authenticated_peer_id(),
            server.authenticated_peer_id()
        );
        let (mut renewed, mut desktop) = reconnect(saved);
        let frame = renewed
            .seal_message(b"synthetic request")
            .expect("encrypt")
            .remove(0);
        assert_eq!(
            desktop.receive_frame(&frame, 0).expect("decrypt"),
            Some(b"synthetic request".to_vec())
        );

        let (mut premature, _) = confirmed_pair();
        assert_eq!(
            premature.seal_message(b"before commit").err(),
            Some(ProtocolError::InvalidState)
        );
        assert!(premature.pending_credential().is_none());
    }

    #[test]
    fn wrong_code_and_replayed_first_flight_never_authenticate() {
        let (mut wrong, hello) = Connection::client_pair(code(*b"000043"));
        let (mut server, reply) = Connection::server(
            ClientHello::decode(&hello).expect("hello"),
            ServerAuth::Pair {
                code: code(*b"000042"),
                peer_id: PeerId::from_bytes([7; 16]),
            },
        )
        .expect("server");
        let first = output(wrong.receive_handshake(&reply).expect("unconfirmed PAKE"));
        assert!(server.receive_handshake(&first).is_err());
        assert!(server.authenticated_peer_id().is_none());

        let (mut client, hello) = Connection::client_pair(code(*b"000042"));
        let hello = ClientHello::decode(&hello).expect("hello");
        let (mut old, reply) = Connection::server(
            hello,
            ServerAuth::Pair {
                code: code(*b"000042"),
                peer_id: PeerId::from_bytes([7; 16]),
            },
        )
        .expect("old");
        let first = output(client.receive_handshake(&reply).expect("first"));
        let _ = old.receive_handshake(&first).expect("old first accepted");
        assert!(old.authenticated_peer_id().is_none());
        let (mut fresh, _) = Connection::server(
            hello,
            ServerAuth::Pair {
                code: code(*b"000042"),
                peer_id: PeerId::from_bytes([7; 16]),
            },
        )
        .expect("fresh");
        assert!(fresh.receive_handshake(&first).is_err());
        assert!(fresh.authenticated_peer_id().is_none());

        let (mut client, hello) = Connection::client_reconnect(
            PeerId::from_bytes([7; 16]),
            SessionSecret::from_storage([9; 32]),
        );
        let (mut server, reply) = Connection::server(
            ClientHello::decode(&hello).expect("hello"),
            ServerAuth::Reconnect(SessionSecret::from_storage([8; 32])),
        )
        .expect("server");
        let first = output(
            client
                .receive_handshake(&reply)
                .expect("unconfirmed first flight"),
        );
        assert!(server.receive_handshake(&first).is_err());
        assert!(server.authenticated_peer_id().is_none());
    }

    #[test]
    fn transcript_binds_version_range_mode_and_peer_identity() {
        let mut incompatible = ClientHello::pair().encode();
        incompatible[1..3].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            ClientHello::decode(&incompatible).err(),
            Some(ProtocolError::IncompatibleVersion)
        );

        // Even an otherwise compatible advertised-range edit changes the authenticated transcript.
        let peer = PeerId::from_bytes([7; 16]);
        let (mut client, mut hello) =
            Connection::client_reconnect(peer, SessionSecret::from_storage([9; 32]));
        hello[3..5].copy_from_slice(&2_u16.to_be_bytes());
        let (mut server, reply) = Connection::server(
            ClientHello::decode(&hello).expect("compatible range"),
            ServerAuth::Reconnect(SessionSecret::from_storage([9; 32])),
        )
        .expect("server");
        let first = output(client.receive_handshake(&reply).expect("first"));
        assert!(server.receive_handshake(&first).is_err());

        for index in [3, 4] {
            let (mut client, hello) =
                Connection::client_reconnect(peer, SessionSecret::from_storage([9; 32]));
            let (_, mut reply) = Connection::server(
                ClientHello::decode(&hello).expect("hello"),
                ServerAuth::Reconnect(SessionSecret::from_storage([9; 32])),
            )
            .expect("server");
            reply[index] ^= 1;
            assert!(client.receive_handshake(&reply).is_err());
        }
    }

    #[test]
    fn fresh_sessions_reject_old_records_and_tamper_or_repetition_is_terminal() {
        let (mut old, _) = reconnect([9; 32]);
        let old_frame = old.seal_message(b"message").expect("old frame").remove(0);
        let (mut fresh, mut server) = reconnect([9; 32]);
        let frame = fresh
            .seal_message(b"message")
            .expect("fresh frame")
            .remove(0);
        assert_ne!(old_frame, frame);
        assert!(server.receive_frame(&old_frame, 0).is_err());
        assert!(server.receive_frame(&frame, 0).is_err());

        let (mut client, mut server) = reconnect([9; 32]);
        let frame = client.seal_message(b"message").expect("frame").remove(0);
        assert!(server.receive_frame(&frame, 0).expect("first").is_some());
        assert!(server.receive_frame(&frame, 1).is_err());
        assert!(server.authenticated_peer_id().is_none());

        let (mut client, mut server) = reconnect([9; 32]);
        let mut frame = client.seal_message(b"message").expect("frame").remove(0);
        frame[0] ^= 1;
        assert!(server.receive_frame(&frame, 0).is_err());
        assert!(server.authenticated_peer_id().is_none());
    }

    #[test]
    fn broker_sized_message_reassembles_only_after_last_bounded_record() {
        let (mut client, mut server) = reconnect([9; 32]);
        // A 16 MiB broker result plus an application envelope must fit the logical-message cap.
        let message = vec![42; 16 * 1024 * 1024 + 128];
        let frames = client.seal_message(&message).expect("fragment");
        assert!(frames.len() > 1);
        assert_eq!(frames[0].len(), MAX_RECORD_LEN);
        for frame in &frames[..frames.len() - 1] {
            assert!(frame.len() <= MAX_RECORD_LEN);
            assert!(server.receive_frame(frame, 10).expect("partial").is_none());
        }
        assert_eq!(
            server
                .receive_frame(frames.last().expect("last"), 11)
                .expect("complete"),
            Some(message)
        );
        let reply = server.seal_message(b"reply").expect("reply").remove(0);
        assert_eq!(
            client
                .receive_frame(&reply, 11)
                .expect("independent direction"),
            Some(b"reply".to_vec())
        );
    }

    #[test]
    fn malformed_over_limit_interleaved_and_expired_assemblies_fail_closed() {
        for (total, offset, fragment) in [
            (MAX_MESSAGE_LEN + 1, 0_u32, b"x".as_slice()),
            (3, 1, b"x".as_slice()),
            (0, 0, b"x".as_slice()),
        ] {
            let (mut client, mut server) = reconnect([9; 32]);
            let mut plain = vec![4];
            plain.extend_from_slice(&0_u64.to_be_bytes());
            plain.extend_from_slice(&(total as u32).to_be_bytes());
            plain.extend_from_slice(&offset.to_be_bytes());
            plain.extend_from_slice(fragment);
            let frame = client
                .encrypt_control(&plain)
                .expect("malformed authenticated frame");
            assert!(server.receive_frame(&frame, 0).is_err());
            assert!(server.authenticated_peer_id().is_none());
        }
        let (mut client, mut server) = reconnect([9; 32]);
        let frames = client
            .seal_message(&vec![1; MAX_RECORD_LEN * 3])
            .expect("fragments");
        assert!(
            server
                .receive_frame(&frames[0], 100)
                .expect("first")
                .is_none()
        );
        assert!(
            server
                .receive_frame(&frames[1], 100 + ASSEMBLY_TIMEOUT_MS - 1)
                .expect("second")
                .is_none()
        );
        assert_eq!(
            server.expire_assembly(100 + ASSEMBLY_TIMEOUT_MS),
            Err(ProtocolError::Expired)
        );
        assert!(
            server
                .receive_frame(&frames[2], 100 + ASSEMBLY_TIMEOUT_MS)
                .is_err()
        );

        // Encrypt a correctly sequenced Noise record that falsely starts another logical message.
        let mut plain = vec![4];
        plain.extend_from_slice(&1_u64.to_be_bytes());
        plain.extend_from_slice(&1_u32.to_be_bytes());
        plain.extend_from_slice(&0_u32.to_be_bytes());
        plain.push(1);
        let (mut client, mut server) = reconnect([9; 32]);
        let mut partial = plain.clone();
        partial[1..9].copy_from_slice(&0_u64.to_be_bytes());
        partial[9..13].copy_from_slice(&2_u32.to_be_bytes());
        let frame = client.encrypt_control(&partial).expect("partial record");
        assert!(server.receive_frame(&frame, 0).expect("partial").is_none());
        let frame = client.encrypt_control(&plain).expect("interleaved record");
        assert!(server.receive_frame(&frame, 1).is_err());
    }
}
