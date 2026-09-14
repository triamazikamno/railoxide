use alloy::consensus::SignableTransaction;
use alloy::primitives::{Signature, TxKind, U256, normalize_v};
use trezor_client::TrezorMessage;

use super::super::{HardwareDerivationError, HardwareDeviceKind, HardwarePublicAccountDescriptor};
use super::client::TrezorHardwareDerivationClient;
use super::passphrase::handle_trezor_interaction;

pub(super) const TREZOR_ETHEREUM_TX_CHUNK_SIZE: usize = 1024;

impl TrezorHardwareDerivationClient {
    pub fn sign_transaction(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
        tx: &dyn SignableTransaction<Signature>,
    ) -> Result<Signature, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Trezor {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Trezor transaction signing requires a Trezor descriptor",
            ));
        }
        let request = trezor_sign_request(tx)?;
        let signature = match request {
            TrezorSignRequest::Legacy(request) => {
                self.sign_legacy_transaction(&descriptor.path, request)?
            }
            TrezorSignRequest::Eip1559(request) => {
                self.sign_eip1559_transaction(&descriptor.path, request)?
            }
        };
        Ok(signature)
    }

    pub(super) fn sign_legacy_transaction(
        &mut self,
        path: &[u32],
        mut request: TrezorLegacySignRequest,
    ) -> Result<Signature, HardwareDerivationError> {
        let chain_id = request.chain_id;
        let mut message = trezor_client::protos::EthereumSignTx::new();
        message.address_n = path.to_vec();
        message.set_nonce(request.nonce);
        message.set_gas_price(request.gas_price);
        message.set_gas_limit(request.gas_limit);
        message.set_value(request.value);
        if let Some(chain_id) = chain_id {
            message.set_chain_id(chain_id);
        }
        message.set_to(request.to);
        message.set_data_length(request.data.len() as u32);
        message.set_data_initial_chunk(trezor_ethereum_next_data_chunk(&mut request.data));

        let response = self.trezor_ethereum_signing_response(message, &mut request.data)?;
        trezor_signature_to_alloy(trezor_ethereum_signature_from_response(
            &response, chain_id,
        )?)
    }

    fn sign_eip1559_transaction(
        &mut self,
        path: &[u32],
        mut request: TrezorEip1559SignRequest,
    ) -> Result<Signature, HardwareDerivationError> {
        let chain_id = request.chain_id;
        let mut message = trezor_client::protos::EthereumSignTxEIP1559::new();
        message.address_n = path.to_vec();
        message.set_nonce(request.nonce);
        message.set_max_gas_fee(request.max_gas_fee);
        message.set_max_priority_fee(request.max_priority_fee);
        message.set_gas_limit(request.gas_limit);
        message.set_value(request.value);
        if let Some(chain_id) = chain_id {
            message.set_chain_id(chain_id);
        }
        message.set_to(request.to);
        if !request.access_list.is_empty() {
            message.access_list = request
                .access_list
                .into_iter()
                .map(
                    |item| trezor_client::protos::ethereum_sign_tx_eip1559::EthereumAccessList {
                        address: Some(item.address),
                        storage_keys: item.storage_keys,
                        ..Default::default()
                    },
                )
                .collect();
        }
        message.set_data_length(request.data.len() as u32);
        message.set_data_initial_chunk(trezor_ethereum_next_data_chunk(&mut request.data));

        let response = self.trezor_ethereum_signing_response(message, &mut request.data)?;
        trezor_signature_to_alloy(trezor_ethereum_signature_from_response(
            &response, chain_id,
        )?)
    }

    fn trezor_ethereum_signing_response<S: TrezorMessage>(
        &mut self,
        message: S,
        data: &mut Vec<u8>,
    ) -> Result<trezor_client::protos::EthereumTxRequest, HardwareDerivationError> {
        let Self {
            client,
            passphrase,
            pin_matrix_provider,
        } = self;
        let response = client.call(
            message,
            Box::new(|_, message: trezor_client::protos::EthereumTxRequest| Ok(message)),
        )?;
        let response =
            handle_trezor_interaction(response, passphrase, pin_matrix_provider.as_ref());
        passphrase.clear_app_passphrase();
        let mut response = response?;
        while response.data_length() > 0 {
            let mut ack = trezor_client::protos::EthereumTxAck::new();
            ack.set_data_chunk(trezor_ethereum_next_data_chunk(data));
            let next = client.call(
                ack,
                Box::new(|_, message: trezor_client::protos::EthereumTxRequest| Ok(message)),
            )?;
            response = handle_trezor_interaction(next, passphrase, pin_matrix_provider.as_ref())?;
        }
        passphrase.clear_app_passphrase();
        Ok(response)
    }

    pub fn sign_message(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
        message: &[u8],
    ) -> Result<Signature, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Trezor {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Trezor message signing requires a Trezor descriptor",
            ));
        }
        let mut request = trezor_client::protos::EthereumSignMessage::new();
        request.address_n.clone_from(&descriptor.path);
        request.set_message(message.to_vec());
        let Self {
            client,
            passphrase,
            pin_matrix_provider,
        } = self;
        let response = client.call(
            request,
            Box::new(
                |_, message: trezor_client::protos::EthereumMessageSignature| {
                    let signature = message.signature();
                    if signature.len() != 65 {
                        return Err(trezor_client::Error::MalformedSignature);
                    }
                    let r: [u8; 32] = signature
                        .get(0..32)
                        .and_then(|bytes| bytes.try_into().ok())
                        .ok_or(trezor_client::Error::MalformedSignature)?;
                    let s: [u8; 32] = signature
                        .get(32..64)
                        .and_then(|bytes| bytes.try_into().ok())
                        .ok_or(trezor_client::Error::MalformedSignature)?;
                    let v = *signature
                        .get(64)
                        .ok_or(trezor_client::Error::MalformedSignature)?;
                    Ok(trezor_client::client::Signature {
                        r,
                        s,
                        v: u64::from(v),
                    })
                },
            ),
        )?;
        let signature =
            handle_trezor_interaction(response, passphrase, pin_matrix_provider.as_ref());
        passphrase.clear_app_passphrase();
        let signature = signature?;
        trezor_signature_to_alloy(signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TrezorLegacySignRequest {
    pub(super) nonce: Vec<u8>,
    pub(super) gas_price: Vec<u8>,
    pub(super) gas_limit: Vec<u8>,
    pub(super) to: String,
    pub(super) value: Vec<u8>,
    pub(super) data: Vec<u8>,
    pub(super) chain_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrezorEip1559SignRequest {
    nonce: Vec<u8>,
    gas_limit: Vec<u8>,
    to: String,
    value: Vec<u8>,
    data: Vec<u8>,
    chain_id: Option<u64>,
    max_gas_fee: Vec<u8>,
    max_priority_fee: Vec<u8>,
    access_list: Vec<trezor_client::client::AccessListItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrezorSignRequest {
    Legacy(TrezorLegacySignRequest),
    Eip1559(TrezorEip1559SignRequest),
}

fn trezor_sign_request(
    tx: &dyn SignableTransaction<Signature>,
) -> Result<TrezorSignRequest, HardwareDerivationError> {
    let nonce = u64_to_trezor(tx.nonce());
    let gas_limit = u64_to_trezor(tx.gas_limit());
    let to = match tx.kind() {
        TxKind::Call(to) => to.to_checksum(None),
        TxKind::Create => String::new(),
    };
    let value = u256_to_trezor(tx.value());
    let data = tx.input().to_vec();
    let chain_id = tx.chain_id();

    if tx.is_eip1559() {
        let access_list = tx
            .access_list()
            .map(|access_list| {
                access_list
                    .0
                    .iter()
                    .map(|item| trezor_client::client::AccessListItem {
                        address: item.address.to_checksum(None),
                        storage_keys: item.storage_keys.iter().map(|key| key.to_vec()).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(TrezorSignRequest::Eip1559(TrezorEip1559SignRequest {
            nonce,
            gas_limit,
            to,
            value,
            data,
            chain_id,
            max_gas_fee: u128_to_trezor(tx.max_fee_per_gas()),
            max_priority_fee: u128_to_trezor(tx.max_priority_fee_per_gas().unwrap_or_default()),
            access_list,
        }))
    } else if tx.is_legacy() {
        Ok(TrezorSignRequest::Legacy(TrezorLegacySignRequest {
            nonce,
            gas_price: u128_to_trezor(tx.max_fee_per_gas()),
            gas_limit,
            to,
            value,
            data,
            chain_id,
        }))
    } else {
        Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Trezor only supports legacy and EIP-1559 transaction signing",
        ))
    }
}

fn trezor_ethereum_next_data_chunk(data: &mut Vec<u8>) -> Vec<u8> {
    let chunk_len = TREZOR_ETHEREUM_TX_CHUNK_SIZE.min(data.len());
    data.drain(..chunk_len).collect()
}

fn trezor_ethereum_signature_from_response(
    response: &trezor_client::protos::EthereumTxRequest,
    chain_id: Option<u64>,
) -> Result<trezor_client::client::Signature, HardwareDerivationError> {
    let mut v = u64::from(response.signature_v());
    if let Some(chain_id) = chain_id
        && v <= 1
    {
        v += 2 * chain_id + 35;
    }
    let r = response.signature_r().try_into().map_err(|_| {
        HardwareDerivationError::UnexpectedResponseLength {
            got: response.signature_r().len(),
            expected: 32,
        }
    })?;
    let s = response.signature_s().try_into().map_err(|_| {
        HardwareDerivationError::UnexpectedResponseLength {
            got: response.signature_s().len(),
            expected: 32,
        }
    })?;
    Ok(trezor_client::client::Signature { r, s, v })
}

fn u64_to_trezor(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    bytes[value.leading_zeros() as usize / 8..].to_vec()
}

fn u128_to_trezor(value: u128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    bytes[value.leading_zeros() as usize / 8..].to_vec()
}

fn u256_to_trezor(value: U256) -> Vec<u8> {
    let bytes = value.to_be_bytes::<32>();
    bytes[value.leading_zeros() / 8..].to_vec()
}

pub(super) fn trezor_signature_to_alloy(
    signature: trezor_client::client::Signature,
) -> Result<Signature, HardwareDerivationError> {
    let parity =
        normalize_v(signature.v).ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
            "Trezor signature has invalid recovery id",
        ))?;
    Ok(Signature::new(
        U256::from_be_bytes(signature.r),
        U256::from_be_bytes(signature.s),
        parity,
    ))
}

#[cfg(test)]
mod tests {
    use alloy::consensus::TxEip1559;
    use alloy::primitives::{Bytes, TxKind, U256};

    use super::{TrezorSignRequest, trezor_sign_request};

    #[test]
    fn trezor_request_preserves_contract_creation_and_input() {
        let input = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00]);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 2,
            gas_limit: 100_000,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 2,
            to: TxKind::Create,
            value: U256::from(7_u64),
            input: input.clone(),
            ..Default::default()
        };

        let request = trezor_sign_request(&tx).expect("Trezor creation request");
        let TrezorSignRequest::Eip1559(request) = request else {
            panic!("expected EIP-1559 request")
        };
        assert!(request.to.is_empty());
        assert_eq!(request.data, input);
        assert_eq!(request.value, vec![7]);
    }
}
