use zeroize::Zeroizing;

use crate::ProtocolError;

pub const MAX_RECORD_LEN: usize = 65_535;
pub const MAX_MESSAGE_LEN: usize = 32 * 1024 * 1024;
pub const ASSEMBLY_TIMEOUT_MS: u64 = 10_000;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 17;
const FRAGMENT_LEN: usize = MAX_RECORD_LEN - TAG_LEN - HEADER_LEN;

struct Assembly {
    message_id: u64,
    total: usize,
    started_ms: u64,
    bytes: Zeroizing<Vec<u8>>,
}

pub(super) struct Records {
    transport: snow::TransportState,
    send_id: u64,
    receive_id: u64,
    assembly: Option<Assembly>,
    last_now_ms: Option<u64>,
}

impl Records {
    pub(super) const fn new(transport: snow::TransportState) -> Self {
        Self {
            transport,
            send_id: 0,
            receive_id: 0,
            assembly: None,
            last_now_ms: None,
        }
    }

    pub(super) fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        if plaintext.len() > MAX_RECORD_LEN - TAG_LEN {
            return Err(ProtocolError::ResourceLimit);
        }
        let mut ciphertext = vec![0; plaintext.len() + TAG_LEN];
        let len = self
            .transport
            .write_message(plaintext, &mut ciphertext)
            .map_err(|_| ProtocolError::AuthenticationFailed)?;
        ciphertext.truncate(len);
        Ok(ciphertext)
    }

    pub(super) fn decrypt(
        &mut self,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, ProtocolError> {
        if !(TAG_LEN..=MAX_RECORD_LEN).contains(&ciphertext.len()) {
            return Err(ProtocolError::ResourceLimit);
        }
        let mut plaintext = Zeroizing::new(vec![0; ciphertext.len()]);
        let len = self
            .transport
            .read_message(ciphertext, &mut plaintext)
            .map_err(|_| ProtocolError::AuthenticationFailed)?;
        plaintext.truncate(len);
        Ok(plaintext)
    }

    pub(super) fn seal(&mut self, message: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        if message.len() > MAX_MESSAGE_LEN {
            return Err(ProtocolError::ResourceLimit);
        }
        let mut frames = Vec::with_capacity(message.len().div_ceil(FRAGMENT_LEN).max(1));
        let mut offset = 0;
        loop {
            let end = (offset + FRAGMENT_LEN).min(message.len());
            let mut plaintext = Zeroizing::new(Vec::with_capacity(HEADER_LEN + end - offset));
            plaintext.push(4);
            plaintext.extend_from_slice(&self.send_id.to_be_bytes());
            plaintext.extend_from_slice(&(message.len() as u32).to_be_bytes());
            plaintext.extend_from_slice(&(offset as u32).to_be_bytes());
            plaintext.extend_from_slice(&message[offset..end]);
            frames.push(self.encrypt(&plaintext)?);
            offset = end;
            if offset == message.len() {
                break;
            }
        }
        self.send_id = self
            .send_id
            .checked_add(1)
            .ok_or(ProtocolError::ResourceLimit)?;
        Ok(frames)
    }

    pub(super) fn receive(
        &mut self,
        frame: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        self.expire(now_ms)?;
        let plaintext = self.decrypt(frame)?;
        if plaintext.len() < HEADER_LEN || plaintext[0] != 4 {
            return Err(ProtocolError::InvalidMessage);
        }
        let message_id = u64::from_be_bytes(
            plaintext[1..9]
                .try_into()
                .map_err(|_| ProtocolError::InvalidMessage)?,
        );
        let total = u32::from_be_bytes(
            plaintext[9..13]
                .try_into()
                .map_err(|_| ProtocolError::InvalidMessage)?,
        ) as usize;
        let offset = u32::from_be_bytes(
            plaintext[13..17]
                .try_into()
                .map_err(|_| ProtocolError::InvalidMessage)?,
        ) as usize;
        let fragment = &plaintext[HEADER_LEN..];
        if total > MAX_MESSAGE_LEN {
            return Err(ProtocolError::ResourceLimit);
        }
        if message_id != self.receive_id
            || offset > total
            || fragment.len() > total - offset
            || (fragment.is_empty() && total != 0)
        {
            return Err(ProtocolError::InvalidMessage);
        }
        if let Some(assembly) = &self.assembly {
            if assembly.message_id != message_id
                || assembly.total != total
                || assembly.bytes.len() != offset
            {
                return Err(ProtocolError::InvalidMessage);
            }
        } else {
            if offset != 0 {
                return Err(ProtocolError::InvalidMessage);
            }
            self.assembly = Some(Assembly {
                message_id,
                total,
                started_ms: now_ms,
                bytes: Zeroizing::new(Vec::with_capacity(total)),
            });
        }
        let assembly = self.assembly.as_mut().ok_or(ProtocolError::InvalidState)?;
        assembly.bytes.extend_from_slice(fragment);
        if assembly.bytes.len() == total {
            let mut assembly = self.assembly.take().ok_or(ProtocolError::InvalidState)?;
            self.receive_id = self
                .receive_id
                .checked_add(1)
                .ok_or(ProtocolError::ResourceLimit)?;
            Ok(Some(std::mem::take(&mut *assembly.bytes)))
        } else {
            Ok(None)
        }
    }

    pub(super) fn expire(&mut self, now_ms: u64) -> Result<(), ProtocolError> {
        if self.last_now_ms.is_some_and(|previous| now_ms < previous) {
            return Err(ProtocolError::Expired);
        }
        self.last_now_ms = Some(now_ms);
        if self.assembly.as_ref().is_some_and(|assembly| {
            now_ms.saturating_sub(assembly.started_ms) >= ASSEMBLY_TIMEOUT_MS
        }) {
            return Err(ProtocolError::Expired);
        }
        Ok(())
    }
}
