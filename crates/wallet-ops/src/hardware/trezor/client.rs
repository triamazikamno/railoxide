use alloy::primitives::Address;
use async_trait::async_trait;
use zeroize::{Zeroize, Zeroizing};

use super::super::{
    ConfirmedHardwarePublicAccount, HardwareAccountPublicKey, HardwareAppVersion,
    HardwareDerivationClient, HardwareDerivationDescriptor, HardwareDerivationError,
    HardwareDerivationMethod, HardwareDeviceKind, HardwareOperationOutput,
    HardwarePublicAccountDescriptor, HardwareTypedDataSigningMode, hardware_profile_fingerprint,
    hardware_public_account_key_path,
};
use super::bridge::BridgeTransport;
use super::passphrase::{
    TrezorPassphraseState, TrezorPinMatrixProvider, handle_trezor_interaction,
};
use super::transport::{trezor_call_raw_with_interactions, trezor_decode_message};
use super::typed_data::classify_trezor_typed_data_signing_mode;
use crate::vault::{HardwareProfileBinding, HardwareProfileSession, TrezorPassphraseMode};
use trezor_client::protos::MessageType;

const TREZOR_CIPHER_INPUT_V1: [u8; 32] = [0u8; 32];

pub struct TrezorHardwareDerivationClient {
    pub(super) client: trezor_client::Trezor,
    pub(super) passphrase: TrezorPassphraseState,
    pub(super) pin_matrix_provider: Option<TrezorPinMatrixProvider>,
}

#[derive(Debug, Clone)]
pub struct TrezorDeviceInfo {
    pub model: String,
    pub vendor: String,
    pub version: HardwareAppVersion,
    pub initialized: bool,
    pub unlocked: Option<bool>,
    pub passphrase_protection: bool,
    pub passphrase_always_on_device: bool,
    pub bootloader_mode: bool,
}

impl TrezorHardwareDerivationClient {
    pub fn connect() -> Result<Self, HardwareDerivationError> {
        match BridgeTransport::connect_unique() {
            Ok(transport) => {
                let mut client = trezor_client::client::trezor_with_transport(
                    trezor_client::Model::Trezor,
                    Box::new(transport),
                );
                client.init_device(None)?;
                Ok(Self {
                    client,
                    passphrase: TrezorPassphraseState::default(),
                    pin_matrix_provider: None,
                })
            }
            Err(error) if error.should_fallback() => {
                tracing::debug!(%error, "Trezor Bridge unavailable; falling back to direct WebUSB transport");
                Self::connect_direct()
            }
            Err(error) => Err(error.into_hardware_error()),
        }
    }

    fn connect_direct() -> Result<Self, HardwareDerivationError> {
        let mut client = trezor_client::unique(false)?;
        client.init_device(None)?;
        Ok(Self {
            client,
            passphrase: TrezorPassphraseState::default(),
            pin_matrix_provider: None,
        })
    }

    pub fn connect_with_session(
        session_id: Option<Vec<u8>>,
    ) -> Result<Self, HardwareDerivationError> {
        match BridgeTransport::connect_unique() {
            Ok(transport) => {
                let mut client = trezor_client::client::trezor_with_transport(
                    trezor_client::Model::Trezor,
                    Box::new(transport),
                );
                client.init_device(session_id)?;
                Ok(Self {
                    client,
                    passphrase: TrezorPassphraseState::default(),
                    pin_matrix_provider: None,
                })
            }
            Err(error) if error.should_fallback() => {
                tracing::debug!(%error, "Trezor Bridge unavailable; falling back to direct WebUSB transport");
                let mut client = trezor_client::unique(false)?;
                client.init_device(session_id)?;
                Ok(Self {
                    client,
                    passphrase: TrezorPassphraseState::default(),
                    pin_matrix_provider: None,
                })
            }
            Err(error) => Err(error.into_hardware_error()),
        }
    }

    pub fn set_passphrase_mode(&mut self, mode: TrezorPassphraseMode) {
        self.passphrase.set_mode(mode);
    }

    pub fn set_app_passphrase(&mut self, passphrase: String) {
        self.passphrase.set_app_passphrase(passphrase);
    }

    pub fn set_app_passphrase_zeroizing(&mut self, passphrase: Zeroizing<String>) {
        self.passphrase.set_app_passphrase_zeroizing(passphrase);
    }

    pub fn set_pin_matrix_provider(&mut self, provider: TrezorPinMatrixProvider) {
        self.pin_matrix_provider = Some(provider);
    }

    pub fn device_info(&self) -> Result<TrezorDeviceInfo, HardwareDerivationError> {
        let features = self
            .client
            .features()
            .ok_or(HardwareDerivationError::InvalidDescriptor(
                "Trezor features were not loaded",
            ))?;
        Ok(TrezorDeviceInfo {
            model: features.model().to_owned(),
            vendor: features.vendor().to_owned(),
            version: HardwareAppVersion::new(
                u16::try_from(features.major_version()).unwrap_or(u16::MAX),
                u16::try_from(features.minor_version()).unwrap_or(u16::MAX),
                u16::try_from(features.patch_version()).unwrap_or(u16::MAX),
            ),
            initialized: features.initialized(),
            unlocked: features.has_unlocked().then(|| features.unlocked()),
            passphrase_protection: features.passphrase_protection(),
            passphrase_always_on_device: features.passphrase_always_on_device(),
            bootloader_mode: features.bootloader_mode(),
        })
    }

    pub fn typed_data_signing_mode(
        &self,
    ) -> Result<HardwareTypedDataSigningMode, HardwareDerivationError> {
        Ok(classify_trezor_typed_data_signing_mode(
            &self.device_info()?,
        ))
    }

    #[must_use]
    pub fn session_id(&self) -> Option<Vec<u8>> {
        self.client.features().and_then(|features| {
            features
                .has_session_id()
                .then(|| features.session_id().to_vec())
                .filter(|session_id| !session_id.is_empty())
        })
    }

    pub fn ethereum_address(&mut self, path: &[u32]) -> Result<String, HardwareDerivationError> {
        self.ethereum_address_with_confirmation(path, false)
    }

    fn ethereum_address_with_confirmation(
        &mut self,
        path: &[u32],
        display_and_confirm: bool,
    ) -> Result<String, HardwareDerivationError> {
        let request = trezor_ethereum_get_address_request(path, display_and_confirm);
        let Self {
            client,
            passphrase,
            pin_matrix_provider,
        } = self;
        let response = client.call(
            request,
            Box::new(|_, message: trezor_client::protos::EthereumAddress| {
                Ok(message.address().to_owned())
            }),
        )?;
        let address = handle_trezor_interaction(response, passphrase, pin_matrix_provider.as_ref());
        passphrase.clear_app_passphrase();
        let address = address?;
        Ok(address.to_ascii_lowercase())
    }

    /// Reads the public key and chain code at `m/44'/60'/{wallet}'/0` without
    /// on-device display.
    pub fn account_public_key(
        &mut self,
        wallet_account_index: u32,
    ) -> Result<HardwareAccountPublicKey, HardwareDerivationError> {
        let path = hardware_public_account_key_path(wallet_account_index)?;
        let request = trezor_ethereum_get_public_key_request(&path);
        let Self {
            client,
            passphrase,
            pin_matrix_provider,
        } = self;
        // `call` traces every decoded response, which here would log the account key.
        let response = trezor_call_raw_with_interactions(
            client,
            request,
            passphrase,
            pin_matrix_provider.as_ref(),
        );
        passphrase.clear_app_passphrase();
        let response = response?;
        if response.message_type() != MessageType::MessageType_EthereumPublicKey {
            return Err(
                trezor_client::Error::UnexpectedMessageType(response.message_type()).into(),
            );
        }
        let message: trezor_client::protos::EthereumPublicKey = trezor_decode_message(response)?;
        trezor_account_public_key_from_node(&message.node.unwrap_or_default())
    }

    pub fn public_ethereum_address(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<Address, HardwareDerivationError> {
        self.public_ethereum_address_with_confirmation(descriptor, false)
    }

    pub fn confirmed_public_ethereum_address(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<Address, HardwareDerivationError> {
        self.public_ethereum_address_with_confirmation(descriptor, true)
    }

    pub fn confirmed_public_ethereum_account(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<ConfirmedHardwarePublicAccount, HardwareDerivationError> {
        let address = self.confirmed_public_ethereum_address(descriptor)?;
        Ok(ConfirmedHardwarePublicAccount::new(
            descriptor.clone(),
            address,
        ))
    }

    fn public_ethereum_address_with_confirmation(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
        display_and_confirm: bool,
    ) -> Result<Address, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Trezor {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Trezor public account requires a Trezor descriptor",
            ));
        }
        self.ethereum_address_with_confirmation(&descriptor.path, display_and_confirm)?
            .parse()
            .map_err(|_| {
                HardwareDerivationError::UnexpectedHardwareResponse(
                    "Trezor address response is not an EVM address",
                )
            })
    }

    pub fn profile_fingerprint(&mut self, path: &[u32]) -> Result<String, HardwareDerivationError> {
        let address = self.ethereum_address(path)?;
        Ok(hardware_profile_fingerprint(
            HardwareDeviceKind::Trezor,
            address,
        ))
    }

    pub fn active_profile_session(
        &mut self,
        path: &[u32],
    ) -> Result<HardwareProfileSession, HardwareDerivationError> {
        let fingerprint = self.profile_fingerprint(path)?;
        let mut session = HardwareProfileSession::unmatched(
            HardwareDeviceKind::Trezor,
            HardwareProfileBinding::evm_address_fingerprint(fingerprint),
            self.session_id(),
        );
        session.set_trezor_passphrase_mode(self.passphrase.mode);
        Ok(session)
    }

    pub fn cipher_key_value(
        &mut self,
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<HardwareOperationOutput, HardwareDerivationError> {
        let mut request = trezor_client::protos::CipherKeyValue::new();
        request.address_n.clone_from(&descriptor.path);
        request.set_key(trezor_cipher_label(descriptor));
        request.set_value(TREZOR_CIPHER_INPUT_V1.to_vec());
        request.set_encrypt(true);
        request.set_ask_on_encrypt(true);
        request.set_ask_on_decrypt(true);

        let Self {
            client,
            passphrase,
            pin_matrix_provider,
        } = self;
        let response = client.call(
            request,
            Box::new(|_, mut message: trezor_client::protos::CipheredKeyValue| {
                Ok(message.take_value())
            }),
        )?;
        let data = handle_trezor_interaction(response, passphrase, pin_matrix_provider.as_ref());
        passphrase.clear_app_passphrase();
        let mut data = data?;
        if data.len() != 32 {
            return Err(HardwareDerivationError::UnexpectedResponseLength {
                got: data.len(),
                expected: 32,
            });
        }
        let mut output = [0u8; 32];
        output.copy_from_slice(&data);
        data.zeroize();
        Ok(HardwareOperationOutput::new(output))
    }
}

#[must_use]
pub fn trezor_cipher_key_label(account_index: u32) -> String {
    format!("Railgun wallet v1 account {account_index}")
}

fn trezor_cipher_label(descriptor: &HardwareDerivationDescriptor) -> String {
    trezor_cipher_key_label(descriptor.account_index)
}

pub(super) fn trezor_ethereum_get_address_request(
    path: &[u32],
    display_and_confirm: bool,
) -> trezor_client::protos::EthereumGetAddress {
    let mut request = trezor_client::protos::EthereumGetAddress::new();
    request.address_n = path.to_vec();
    request.set_show_display(display_and_confirm);
    request
}

pub(super) fn trezor_ethereum_get_public_key_request(
    path: &[u32],
) -> trezor_client::protos::EthereumGetPublicKey {
    let mut request = trezor_client::protos::EthereumGetPublicKey::new();
    request.address_n = path.to_vec();
    request.set_show_display(false);
    request
}

pub(super) fn trezor_account_public_key_from_node(
    node: &trezor_client::protos::HDNodeType,
) -> Result<HardwareAccountPublicKey, HardwareDerivationError> {
    let chain_code = <[u8; 32]>::try_from(node.chain_code()).map_err(|_| {
        HardwareDerivationError::UnexpectedHardwareResponse(
            "Trezor account key chain code is not 32 bytes",
        )
    })?;
    if node.public_key().len() != 33 {
        return Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Trezor account public key is not 33 bytes",
        ));
    }
    Ok(HardwareAccountPublicKey {
        public_key: node.public_key().to_vec(),
        chain_code,
    })
}

#[async_trait(?Send)]
impl HardwareDerivationClient for TrezorHardwareDerivationClient {
    async fn derive_hardware_output(
        &mut self,
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<HardwareOperationOutput, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.method != HardwareDerivationMethod::TrezorCipherKeyValueV1 {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Trezor client requires a Trezor CipherKeyValue descriptor",
            ));
        }
        self.cipher_key_value(descriptor)
    }
}
