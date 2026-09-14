use std::io::Cursor;

use super::{
    ConfirmedHardwarePublicAccount, HardwareAppVersion, HardwareDerivationClient,
    HardwareDerivationDescriptor, HardwareDerivationError, HardwareDerivationMethod,
    HardwareDeviceKind, HardwareOperationOutput, HardwarePublicAccountDescriptor,
    HardwareTypedDataSigningMode, hardware_profile_fingerprint,
};
use crate::hardware_typed_data::{
    HardwareEip712ArrayElement, HardwareEip712FieldDefinition, HardwareEip712FieldValue,
    HardwareEip712Model, HardwareEip712PrimitiveType, HardwareEip712StructValue,
    HardwareEip712Type, HardwareEip712Value,
};
use crate::vault::{HardwareProfileBinding, HardwareProfileSession};
use alloy::hex;
use alloy::primitives::{Address, Signature, U256, normalize_v};
use async_trait::async_trait;
use coins_ledger::{
    LedgerError,
    common::{APDUAnswer, APDUCommand, APDUData},
    transports::native::NativeTransportError,
};
use hidapi_rusb::{DeviceInfo, HidApi, HidDevice};
use tokio::{sync::Mutex as AsyncMutex, task};

pub const LEDGER_ETHEREUM_EIP1024_MIN_APP_VERSION: HardwareAppVersion =
    HardwareAppVersion::new(1, 9, 17);
pub const LEDGER_ETHEREUM_EIP712_HASH_FALLBACK_MIN_APP_VERSION: HardwareAppVersion =
    HardwareAppVersion::new(1, 5, 0);
pub const LEDGER_ETHEREUM_EIP712_CLEAR_MIN_APP_VERSION: HardwareAppVersion =
    HardwareAppVersion::new(1, 10, 0);

const LEDGER_READY_MESSAGE: &str =
    "Plug it in and enter your PIN, then open the Ethereum app and retry.";
const LEDGER_VID: u16 = 0x2c97;
#[cfg(not(target_os = "linux"))]
const LEDGER_USAGE_PAGE: u16 = 0xffa0;
const LEDGER_CHANNEL: u16 = 0x0101;
const LEDGER_PACKET_WRITE_SIZE: usize = 65;
const LEDGER_PACKET_READ_SIZE: usize = 64;
const LEDGER_TIMEOUT_MS: i32 = 10_000_000;
static LEDGER_IO_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

pub const RAILGUN_LEDGER_EIP1024_REMOTE_PUBLIC_KEY_V1: [u8; 32] = [
    0xeb, 0x88, 0xd6, 0xa7, 0xb6, 0x92, 0x83, 0xd0, 0x58, 0x22, 0x98, 0xe6, 0x04, 0xe1, 0x3e, 0x4d,
    0x86, 0xa2, 0x98, 0xe5, 0x96, 0xe5, 0x82, 0x93, 0xee, 0x6a, 0x8d, 0xbb, 0x07, 0x61, 0x0f, 0x51,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerDeviceModel {
    NanoS,
    NanoX,
    NanoSPlus,
    Stax,
    Flex,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerDeviceInfo {
    pub model: LedgerDeviceModel,
    pub ethereum_app_version: HardwareAppVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEip712Apdu {
    pub ins: u8,
    pub p1: u8,
    pub p2: u8,
    pub data: Vec<u8>,
}

pub(crate) enum LedgerEip712ClearSigningOutcome {
    Signed(Signature),
    Downgrade(HardwareTypedDataSigningMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum LedgerEip712ClearFailureKind {
    CapabilityUnsupportedBeforeConfirmation,
    PayloadUnsupported,
    UserRejected,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerEip712ArrayLevel {
    Dynamic,
    Fixed(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LedgerEip712BaseType {
    Custom(String),
    Int(usize),
    Uint(usize),
    Address,
    Bool,
    String,
    FixedBytes(usize),
    Bytes,
}

pub struct LedgerHardwareDerivationClient {
    request_control: Option<crate::dapp_request::DappRequestControl>,
}

impl LedgerHardwareDerivationClient {
    #[must_use]
    pub(crate) fn with_request_control(
        mut self,
        control: Option<crate::dapp_request::DappRequestControl>,
    ) -> Self {
        self.request_control = control;
        self
    }

    pub async fn connect() -> Result<Self, HardwareDerivationError> {
        let _guard = LEDGER_IO_LOCK.lock().await;
        ledger_hid_preflight()?;
        Ok(Self {
            request_control: None,
        })
    }

    pub async fn ethereum_app_version(
        &self,
    ) -> Result<HardwareAppVersion, HardwareDerivationError> {
        let command = APDUCommand {
            cla: 0xe0,
            ins: 0x06,
            p1: 0x00,
            p2: 0x00,
            data: APDUData::new(&[]),
            response_len: Some(0),
        };
        let answer = ledger_exchange(&command)
            .await
            .map_err(|error| ledger_exchange_error(error, "get Ethereum app version"))?;
        let data = ledger_response_data(&answer, "get Ethereum app version")?;
        if data.len() != 4 {
            return Err(HardwareDerivationError::UnexpectedResponseLength {
                got: data.len(),
                expected: 4,
            });
        }
        Ok(HardwareAppVersion::new(
            u16::from(data[1]),
            u16::from(data[2]),
            u16::from(data[3]),
        ))
    }

    pub async fn device_info(&self) -> Result<LedgerDeviceInfo, HardwareDerivationError> {
        let model = ledger_connected_device_model()?;
        let ethereum_app_version = self.ethereum_app_version().await?;
        Ok(LedgerDeviceInfo {
            model,
            ethereum_app_version,
        })
    }

    pub async fn typed_data_signing_mode(
        &self,
    ) -> Result<HardwareTypedDataSigningMode, HardwareDerivationError> {
        let info = self.device_info().await?;
        Ok(classify_ledger_typed_data_signing_mode(&info))
    }

    pub async fn ethereum_address(&self, path: &[u32]) -> Result<String, HardwareDerivationError> {
        self.ethereum_address_with_confirmation(path, false).await
    }

    async fn ethereum_address_with_confirmation(
        &self,
        path: &[u32],
        display_and_confirm: bool,
    ) -> Result<String, HardwareDerivationError> {
        let data = ledger_path_payload(path)?;
        let command = APDUCommand {
            cla: 0xe0,
            ins: 0x02,
            p1: ledger_address_display_p1(display_and_confirm),
            p2: 0x00,
            data: APDUData::new(&data),
            response_len: None,
        };
        let answer = ledger_exchange(&command)
            .await
            .map_err(|error| ledger_exchange_error(error, "get Ethereum address"))?;
        let data = ledger_response_data(&answer, "get Ethereum address")?;
        let address = ledger_address_from_response(data)?;
        Ok(format!("0x{}", address.to_ascii_lowercase()))
    }

    pub async fn public_ethereum_address(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<Address, HardwareDerivationError> {
        self.public_ethereum_address_with_confirmation(descriptor, false)
            .await
    }

    pub async fn confirmed_public_ethereum_address(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<Address, HardwareDerivationError> {
        self.public_ethereum_address_with_confirmation(descriptor, true)
            .await
    }

    pub async fn confirmed_public_ethereum_account(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<ConfirmedHardwarePublicAccount, HardwareDerivationError> {
        let address = self.confirmed_public_ethereum_address(descriptor).await?;
        Ok(ConfirmedHardwarePublicAccount::new(
            descriptor.clone(),
            address,
        ))
    }

    async fn public_ethereum_address_with_confirmation(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        display_and_confirm: bool,
    ) -> Result<Address, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Ledger {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Ledger public account requires a Ledger descriptor",
            ));
        }
        self.ethereum_address_with_confirmation(&descriptor.path, display_and_confirm)
            .await?
            .parse()
            .map_err(|_| {
                HardwareDerivationError::UnexpectedHardwareResponse(
                    "Ledger address response is not an EVM address",
                )
            })
    }

    pub async fn sign_transaction_rlp(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        encoded_for_signing: &[u8],
    ) -> Result<Signature, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Ledger {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Ledger transaction signing requires a Ledger descriptor",
            ));
        }
        let mut payload = ledger_path_payload(&descriptor.path)?;
        payload.extend_from_slice(encoded_for_signing);
        self.sign_payload(0x04, &payload).await
    }

    pub async fn sign_message(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        message: &[u8],
    ) -> Result<Signature, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.device_kind != HardwareDeviceKind::Ledger {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Ledger message signing requires a Ledger descriptor",
            ));
        }
        let message_len = u32::try_from(message.len()).map_err(|_| {
            HardwareDerivationError::InvalidDescriptor("Ledger message is too large")
        })?;
        let mut payload = ledger_path_payload(&descriptor.path)?;
        payload.extend_from_slice(&message_len.to_be_bytes());
        payload.extend_from_slice(message);
        self.sign_payload(0x08, &payload).await
    }

    #[allow(dead_code)]
    pub(crate) async fn sign_typed_data_clear(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        typed_data: &HardwareEip712Model,
    ) -> Result<Signature, HardwareDerivationError> {
        let apdus = ledger_eip712_clear_signing_apdus(descriptor, typed_data)?;
        self.exchange_eip712_signing_apdus(&apdus, "sign EIP-712 typed data")
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn sign_typed_data_clear_or_downgrade(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        typed_data: &HardwareEip712Model,
    ) -> Result<LedgerEip712ClearSigningOutcome, HardwareDerivationError> {
        let info = self.device_info().await?;
        let apdus = ledger_eip712_clear_signing_apdus(descriptor, typed_data)?;
        self.exchange_eip712_clear_signing_apdus(&apdus, "sign EIP-712 typed data", &info)
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn sign_typed_data_hash(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
        typed_data: &HardwareEip712Model,
    ) -> Result<Signature, HardwareDerivationError> {
        let apdu = ledger_eip712_hash_signing_apdu(descriptor, typed_data)?;
        self.exchange_eip712_signing_apdus(&[apdu], "sign EIP-712 typed-data hashes")
            .await
    }

    #[allow(dead_code)]
    async fn exchange_eip712_signing_apdus(
        &self,
        apdus: &[LedgerEip712Apdu],
        operation: &'static str,
    ) -> Result<Signature, HardwareDerivationError> {
        let mut signature_response = None;
        for apdu in apdus {
            let command = APDUCommand {
                cla: 0xe0,
                ins: apdu.ins,
                p1: apdu.p1,
                p2: apdu.p2,
                data: APDUData::new(&apdu.data),
                response_len: None,
            };
            let answer =
                ledger_signing_exchange(&command, self.request_control.as_ref(), operation).await?;
            ledger_ensure_success(&answer, operation)?;
            if apdu.ins == 0x0c {
                signature_response = Some(answer);
            }
        }
        let answer =
            signature_response.ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
                "Ledger EIP-712 signing sequence did not include a signature command",
            ))?;
        let data = ledger_response_data(&answer, operation)?;
        ledger_signature_from_response(data, operation)
    }

    async fn exchange_eip712_clear_signing_apdus(
        &self,
        apdus: &[LedgerEip712Apdu],
        operation: &'static str,
        info: &LedgerDeviceInfo,
    ) -> Result<LedgerEip712ClearSigningOutcome, HardwareDerivationError> {
        let mut signature_response = None;
        let mut device_confirmation_started = false;
        for apdu in apdus {
            let command = APDUCommand {
                cla: 0xe0,
                ins: apdu.ins,
                p1: apdu.p1,
                p2: apdu.p2,
                data: APDUData::new(&apdu.data),
                response_len: None,
            };
            let answer =
                ledger_signing_exchange(&command, self.request_control.as_ref(), operation).await?;
            if let Err(error) = ledger_ensure_success(&answer, operation) {
                if let Some(mode) = ledger_eip712_clear_failure_downgrade_mode(
                    &error,
                    apdu,
                    device_confirmation_started,
                    *info,
                ) {
                    return Ok(LedgerEip712ClearSigningOutcome::Downgrade(mode));
                }
                return Err(error);
            }
            if ledger_eip712_sign_apdu(apdu) {
                device_confirmation_started = true;
                signature_response = Some(answer);
            }
        }
        let answer =
            signature_response.ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
                "Ledger EIP-712 signing sequence did not include a signature command",
            ))?;
        let data = ledger_response_data(&answer, operation)?;
        ledger_signature_from_response(data, operation).map(LedgerEip712ClearSigningOutcome::Signed)
    }

    async fn sign_payload(
        &self,
        ins: u8,
        payload: &[u8],
    ) -> Result<Signature, HardwareDerivationError> {
        let operation = ledger_signing_operation(ins);
        let mut command = APDUCommand {
            cla: 0xe0,
            ins,
            p1: 0x00,
            p2: 0x00,
            data: APDUData::new(&[]),
            response_len: None,
        };
        let chunk_size = (0..=255)
            .rev()
            .find(|size| payload.len() % size != 3)
            .expect("nonzero Ledger chunk size exists");
        let mut answer = None;
        for chunk in payload.chunks(chunk_size) {
            command.data = APDUData::new(chunk);
            let response =
                ledger_signing_exchange(&command, self.request_control.as_ref(), operation).await?;
            ledger_ensure_success(&response, operation)?;
            answer = Some(response);
            command.p1 = 0x80;
        }
        let answer = answer.ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger signing payload is empty",
        ))?;
        let data = ledger_response_data(&answer, operation)?;
        ledger_signature_from_response(data, operation)
    }

    pub async fn profile_fingerprint(
        &self,
        path: &[u32],
    ) -> Result<String, HardwareDerivationError> {
        let address = self.ethereum_address(path).await?;
        Ok(hardware_profile_fingerprint(
            HardwareDeviceKind::Ledger,
            address,
        ))
    }

    pub async fn active_profile_session(
        &self,
        path: &[u32],
    ) -> Result<HardwareProfileSession, HardwareDerivationError> {
        let fingerprint = self.profile_fingerprint(path).await?;
        Ok(HardwareProfileSession::unmatched(
            HardwareDeviceKind::Ledger,
            HardwareProfileBinding::evm_address_fingerprint(fingerprint),
            None,
        ))
    }

    pub async fn eip1024_shared_secret(
        &self,
        path: &[u32],
        display_and_confirm: bool,
    ) -> Result<HardwareOperationOutput, HardwareDerivationError> {
        let version = self.ethereum_app_version().await?;
        if version < LEDGER_ETHEREUM_EIP1024_MIN_APP_VERSION {
            return Err(
                HardwareDerivationError::UnsupportedLedgerEthereumAppVersion {
                    actual: version,
                    required: LEDGER_ETHEREUM_EIP1024_MIN_APP_VERSION,
                },
            );
        }

        let mut data = ledger_path_payload(path)?;
        data.extend_from_slice(&RAILGUN_LEDGER_EIP1024_REMOTE_PUBLIC_KEY_V1);

        let command = APDUCommand {
            cla: 0xe0,
            ins: 0x18,
            p1: u8::from(display_and_confirm),
            p2: 0x01,
            data: APDUData::new(&data),
            response_len: None,
        };
        let answer = ledger_exchange(&command)
            .await
            .map_err(|error| ledger_exchange_error(error, "derive Railgun secret"))?;
        let data = ledger_response_data(&answer, "derive Railgun secret")?;
        if data.len() != 32 {
            return Err(HardwareDerivationError::UnexpectedResponseLength {
                got: data.len(),
                expected: 32,
            });
        }
        let mut output = [0u8; 32];
        output.copy_from_slice(data);
        Ok(HardwareOperationOutput::new(output))
    }
}

fn ledger_hid_preflight() -> Result<(), HardwareDerivationError> {
    let api = HidApi::new().map_err(|error| {
        tracing::debug!(%error, "Ledger HID preflight failed to initialize HID API");
        HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE)
    })?;
    if api
        .device_list()
        .any(|device| ledger_hid_device_matches(device.vendor_id(), device.usage_page()))
    {
        Ok(())
    } else {
        Err(HardwareDerivationError::LedgerUnavailable(
            LEDGER_READY_MESSAGE,
        ))
    }
}

fn ledger_connected_device_model() -> Result<LedgerDeviceModel, HardwareDerivationError> {
    let api = HidApi::new().map_err(|error| {
        tracing::debug!(%error, "Ledger HID model probe failed to initialize HID API");
        HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE)
    })?;
    let device = api
        .device_list()
        .find(|device| ledger_hid_device_matches(device.vendor_id(), device.usage_page()))
        .ok_or(HardwareDerivationError::LedgerUnavailable(
            LEDGER_READY_MESSAGE,
        ))?;
    Ok(ledger_device_model(
        device.product_id(),
        device.product_string(),
    ))
}

fn ledger_device_model(product_id: u16, product: Option<&str>) -> LedgerDeviceModel {
    if let Some(product) = product {
        let product = product.to_ascii_lowercase();
        if product.contains("nano s plus") || product.contains("nano s+") {
            return LedgerDeviceModel::NanoSPlus;
        }
        if product.contains("nano x") {
            return LedgerDeviceModel::NanoX;
        }
        if product.contains("nano s") {
            return LedgerDeviceModel::NanoS;
        }
        if product.contains("stax") {
            return LedgerDeviceModel::Stax;
        }
        if product.contains("flex") {
            return LedgerDeviceModel::Flex;
        }
    }
    match product_id {
        0x0001 => LedgerDeviceModel::NanoS,
        0x0004 => LedgerDeviceModel::NanoX,
        0x0005 => LedgerDeviceModel::NanoSPlus,
        0x0006 => LedgerDeviceModel::Stax,
        0x0007 => LedgerDeviceModel::Flex,
        _ => LedgerDeviceModel::Unknown,
    }
}

#[must_use]
pub fn classify_ledger_typed_data_signing_mode(
    info: &LedgerDeviceInfo,
) -> HardwareTypedDataSigningMode {
    if matches!(
        info.model,
        LedgerDeviceModel::NanoX | LedgerDeviceModel::NanoSPlus
    ) && info.ethereum_app_version >= LEDGER_ETHEREUM_EIP712_CLEAR_MIN_APP_VERSION
    {
        HardwareTypedDataSigningMode::ClearSign
    } else if info.ethereum_app_version >= LEDGER_ETHEREUM_EIP712_HASH_FALLBACK_MIN_APP_VERSION {
        HardwareTypedDataSigningMode::Eip712HashFallback
    } else {
        HardwareTypedDataSigningMode::Unsupported
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn classify_ledger_eip712_clear_failure(
    error: &HardwareDerivationError,
    failed_apdu: &LedgerEip712Apdu,
    device_confirmation_started: bool,
) -> LedgerEip712ClearFailureKind {
    let HardwareDerivationError::LedgerStatus { status, .. } = error else {
        return LedgerEip712ClearFailureKind::Other;
    };
    if ledger_eip712_user_rejection_status(*status) {
        return LedgerEip712ClearFailureKind::UserRejected;
    }
    if ledger_eip712_payload_rejection_status(*status) {
        return LedgerEip712ClearFailureKind::PayloadUnsupported;
    }
    if !device_confirmation_started
        && !ledger_eip712_sign_apdu(failed_apdu)
        && ledger_eip712_capability_unsupported_status(*status)
    {
        return LedgerEip712ClearFailureKind::CapabilityUnsupportedBeforeConfirmation;
    }
    LedgerEip712ClearFailureKind::Other
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ledger_eip712_clear_failure_downgrade_mode(
    error: &HardwareDerivationError,
    failed_apdu: &LedgerEip712Apdu,
    device_confirmation_started: bool,
    info: LedgerDeviceInfo,
) -> Option<HardwareTypedDataSigningMode> {
    if !matches!(
        classify_ledger_eip712_clear_failure(error, failed_apdu, device_confirmation_started),
        LedgerEip712ClearFailureKind::CapabilityUnsupportedBeforeConfirmation
    ) {
        return None;
    }
    if info.ethereum_app_version >= LEDGER_ETHEREUM_EIP712_HASH_FALLBACK_MIN_APP_VERSION {
        Some(HardwareTypedDataSigningMode::Eip712HashFallback)
    } else {
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
const fn ledger_eip712_sign_apdu(apdu: &LedgerEip712Apdu) -> bool {
    apdu.ins == 0x0c
}

#[cfg_attr(not(test), allow(dead_code))]
const fn ledger_eip712_capability_unsupported_status(status: u16) -> bool {
    matches!(status, 0x6a81 | 0x6a86 | 0x6d00 | 0x6e00)
}

#[cfg_attr(not(test), allow(dead_code))]
const fn ledger_eip712_payload_rejection_status(status: u16) -> bool {
    matches!(status, 0x6a80 | 0x6b00)
}

#[cfg_attr(not(test), allow(dead_code))]
const fn ledger_eip712_user_rejection_status(status: u16) -> bool {
    matches!(status, 0x6982 | 0x6985)
}

pub(crate) fn ledger_eip712_clear_signing_apdus(
    descriptor: &HardwarePublicAccountDescriptor,
    typed_data: &HardwareEip712Model,
) -> Result<Vec<LedgerEip712Apdu>, HardwareDerivationError> {
    descriptor.validate()?;
    if descriptor.device_kind != HardwareDeviceKind::Ledger {
        return Err(HardwareDerivationError::InvalidDescriptor(
            "Ledger typed-data signing requires a Ledger descriptor",
        ));
    }
    let mut apdus = ledger_eip712_definition_apdus(typed_data)?;
    apdus.extend(ledger_eip712_implementation_apdus(typed_data)?);
    apdus.push(LedgerEip712Apdu {
        ins: 0x0c,
        p1: 0x00,
        p2: 0x01,
        data: ledger_path_payload(&descriptor.path)?,
    });
    Ok(apdus)
}

pub(crate) fn ledger_eip712_hash_signing_apdu(
    descriptor: &HardwarePublicAccountDescriptor,
    typed_data: &HardwareEip712Model,
) -> Result<LedgerEip712Apdu, HardwareDerivationError> {
    descriptor.validate()?;
    if descriptor.device_kind != HardwareDeviceKind::Ledger {
        return Err(HardwareDerivationError::InvalidDescriptor(
            "Ledger typed-data signing requires a Ledger descriptor",
        ));
    }
    let message_hash =
        typed_data
            .message_hash()
            .ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
                "Ledger EIP-712 hash fallback requires a message hash",
            ))?;
    let mut data = ledger_path_payload(&descriptor.path)?;
    data.extend_from_slice(typed_data.domain_separator_hash().as_slice());
    data.extend_from_slice(message_hash.as_slice());
    Ok(LedgerEip712Apdu {
        ins: 0x0c,
        p1: 0x00,
        p2: 0x00,
        data,
    })
}

fn ledger_eip712_definition_apdus(
    typed_data: &HardwareEip712Model,
) -> Result<Vec<LedgerEip712Apdu>, HardwareDerivationError> {
    let mut apdus = Vec::new();
    for (type_name, fields) in typed_data.type_definitions() {
        apdus.push(LedgerEip712Apdu {
            ins: 0x1a,
            p1: 0x00,
            p2: 0x00,
            data: ledger_len_prefixed_stringless_payload(type_name)?,
        });
        for field in fields {
            apdus.push(LedgerEip712Apdu {
                ins: 0x1a,
                p1: 0x00,
                p2: 0xff,
                data: ledger_eip712_field_definition_payload(field)?,
            });
        }
    }
    Ok(apdus)
}

fn ledger_eip712_field_definition_payload(
    field: &HardwareEip712FieldDefinition,
) -> Result<Vec<u8>, HardwareDerivationError> {
    let (base, array_levels) = ledger_eip712_type_parts(&field.value_type);
    let has_array = !array_levels.is_empty();
    let type_size = ledger_eip712_type_size(&base);
    let mut descriptor = ledger_eip712_type_code(&base);
    if has_array {
        descriptor |= 0x80;
    }
    if type_size.is_some() {
        descriptor |= 0x40;
    }

    let mut payload = vec![descriptor];
    if let LedgerEip712BaseType::Custom(type_name) = &base {
        push_u8_len_prefixed_bytes(&mut payload, type_name.as_bytes())?;
    }
    if let Some(type_size) = type_size {
        payload.push(u8::try_from(type_size).map_err(|_| {
            HardwareDerivationError::InvalidDescriptor("Ledger EIP-712 type size is too large")
        })?);
    }
    if has_array {
        payload.push(u8::try_from(array_levels.len()).map_err(|_| {
            HardwareDerivationError::InvalidDescriptor("Ledger EIP-712 array nesting is too deep")
        })?);
        for level in array_levels {
            match level {
                LedgerEip712ArrayLevel::Dynamic => payload.push(0),
                LedgerEip712ArrayLevel::Fixed(len) => {
                    payload.push(1);
                    payload.push(u8::try_from(len).map_err(|_| {
                        HardwareDerivationError::InvalidDescriptor(
                            "Ledger EIP-712 fixed array length is too large",
                        )
                    })?);
                }
            }
        }
    }
    push_u8_len_prefixed_bytes(&mut payload, field.name.as_bytes())?;
    Ok(payload)
}

fn ledger_eip712_implementation_apdus(
    typed_data: &HardwareEip712Model,
) -> Result<Vec<LedgerEip712Apdu>, HardwareDerivationError> {
    let mut apdus = Vec::new();
    append_ledger_eip712_struct_implementation(&mut apdus, typed_data.domain())?;
    if let Some(message) = typed_data.message() {
        append_ledger_eip712_struct_implementation(&mut apdus, message)?;
    }
    Ok(apdus)
}

fn append_ledger_eip712_struct_implementation(
    apdus: &mut Vec<LedgerEip712Apdu>,
    value: &HardwareEip712StructValue,
) -> Result<(), HardwareDerivationError> {
    apdus.push(LedgerEip712Apdu {
        ins: 0x1c,
        p1: 0x00,
        p2: 0x00,
        data: ledger_len_prefixed_stringless_payload(&value.type_name)?,
    });
    for field in &value.fields {
        append_ledger_eip712_field_implementation(apdus, field)?;
    }
    Ok(())
}

fn append_ledger_eip712_field_implementation(
    apdus: &mut Vec<LedgerEip712Apdu>,
    field: &HardwareEip712FieldValue,
) -> Result<(), HardwareDerivationError> {
    append_ledger_eip712_value_implementation(apdus, &field.value)
}

fn append_ledger_eip712_value_implementation(
    apdus: &mut Vec<LedgerEip712Apdu>,
    value: &HardwareEip712Value,
) -> Result<(), HardwareDerivationError> {
    match value {
        HardwareEip712Value::Struct(value) => {
            append_ledger_eip712_struct_implementation(apdus, value)
        }
        HardwareEip712Value::DynamicArray(values) | HardwareEip712Value::FixedArray(values) => {
            append_ledger_eip712_array_implementation(apdus, values)
        }
        _ => {
            let value = ledger_eip712_raw_value(value)?;
            let mut payload = Vec::with_capacity(2 + value.len());
            payload.extend_from_slice(
                &u16::try_from(value.len())
                    .map_err(|_| {
                        HardwareDerivationError::InvalidDescriptor(
                            "Ledger EIP-712 field value is too large",
                        )
                    })?
                    .to_be_bytes(),
            );
            payload.extend_from_slice(&value);
            append_ledger_eip712_field_chunks(apdus, payload);
            Ok(())
        }
    }
}

fn append_ledger_eip712_field_chunks(apdus: &mut Vec<LedgerEip712Apdu>, mut payload: Vec<u8>) {
    while !payload.is_empty() {
        let chunk_len = payload.len().min(255);
        let chunk = payload[..chunk_len].to_vec();
        payload.drain(..chunk_len);
        apdus.push(LedgerEip712Apdu {
            ins: 0x1c,
            p1: u8::from(!payload.is_empty()),
            p2: 0xff,
            data: chunk,
        });
    }
}

fn append_ledger_eip712_array_implementation(
    apdus: &mut Vec<LedgerEip712Apdu>,
    values: &[HardwareEip712ArrayElement],
) -> Result<(), HardwareDerivationError> {
    apdus.push(LedgerEip712Apdu {
        ins: 0x1c,
        p1: 0x00,
        p2: 0x0f,
        data: vec![u8::try_from(values.len()).map_err(|_| {
            HardwareDerivationError::InvalidDescriptor("Ledger EIP-712 array is too large")
        })?],
    });
    for value in values {
        append_ledger_eip712_value_implementation(apdus, &value.value)?;
    }
    Ok(())
}

fn ledger_eip712_raw_value(
    value: &HardwareEip712Value,
) -> Result<Vec<u8>, HardwareDerivationError> {
    match value {
        HardwareEip712Value::Bool(value) => Ok(vec![u8::from(*value)]),
        HardwareEip712Value::Int { value, .. } => Ok(i256_to_ledger(value)),
        HardwareEip712Value::Uint { value, .. } => Ok(u256_to_ledger(*value)),
        HardwareEip712Value::Address(value) => Ok(value.as_slice().to_vec()),
        HardwareEip712Value::FixedBytes { bytes, .. } | HardwareEip712Value::Bytes(bytes) => {
            Ok(bytes.clone())
        }
        HardwareEip712Value::String(value) => Ok(value.as_bytes().to_vec()),
        HardwareEip712Value::Struct(_)
        | HardwareEip712Value::DynamicArray(_)
        | HardwareEip712Value::FixedArray(_) => {
            Err(HardwareDerivationError::UnexpectedHardwareResponse(
                "Ledger EIP-712 container value cannot be encoded as a raw field",
            ))
        }
    }
}

fn ledger_eip712_type_parts(
    value_type: &HardwareEip712Type,
) -> (LedgerEip712BaseType, Vec<LedgerEip712ArrayLevel>) {
    match value_type {
        HardwareEip712Type::Primitive(primitive) => {
            (ledger_eip712_primitive_type(primitive), Vec::new())
        }
        HardwareEip712Type::Struct(name) => {
            (LedgerEip712BaseType::Custom(name.clone()), Vec::new())
        }
        HardwareEip712Type::DynamicArray(element) => {
            let (base, mut levels) = ledger_eip712_type_parts(element);
            levels.push(LedgerEip712ArrayLevel::Dynamic);
            (base, levels)
        }
        HardwareEip712Type::FixedArray { element, len } => {
            let (base, mut levels) = ledger_eip712_type_parts(element);
            levels.push(LedgerEip712ArrayLevel::Fixed(*len));
            (base, levels)
        }
    }
}

const fn ledger_eip712_primitive_type(
    primitive: &HardwareEip712PrimitiveType,
) -> LedgerEip712BaseType {
    match primitive {
        HardwareEip712PrimitiveType::Bool => LedgerEip712BaseType::Bool,
        HardwareEip712PrimitiveType::Int(bits) => LedgerEip712BaseType::Int(*bits),
        HardwareEip712PrimitiveType::Uint(bits) => LedgerEip712BaseType::Uint(*bits),
        HardwareEip712PrimitiveType::Address => LedgerEip712BaseType::Address,
        HardwareEip712PrimitiveType::FixedBytes(size) => LedgerEip712BaseType::FixedBytes(*size),
        HardwareEip712PrimitiveType::Bytes => LedgerEip712BaseType::Bytes,
        HardwareEip712PrimitiveType::String => LedgerEip712BaseType::String,
    }
}

const fn ledger_eip712_type_code(base: &LedgerEip712BaseType) -> u8 {
    match base {
        LedgerEip712BaseType::Custom(_) => 0,
        LedgerEip712BaseType::Int(_) => 1,
        LedgerEip712BaseType::Uint(_) => 2,
        LedgerEip712BaseType::Address => 3,
        LedgerEip712BaseType::Bool => 4,
        LedgerEip712BaseType::String => 5,
        LedgerEip712BaseType::FixedBytes(_) => 6,
        LedgerEip712BaseType::Bytes => 7,
    }
}

fn ledger_eip712_type_size(base: &LedgerEip712BaseType) -> Option<usize> {
    match base {
        LedgerEip712BaseType::Int(bits) | LedgerEip712BaseType::Uint(bits) => Some(bits / 8),
        LedgerEip712BaseType::FixedBytes(size) => Some(*size),
        LedgerEip712BaseType::Custom(_)
        | LedgerEip712BaseType::Address
        | LedgerEip712BaseType::Bool
        | LedgerEip712BaseType::String
        | LedgerEip712BaseType::Bytes => None,
    }
}

fn push_u8_len_prefixed_bytes(
    output: &mut Vec<u8>,
    value: &[u8],
) -> Result<(), HardwareDerivationError> {
    output.push(u8::try_from(value.len()).map_err(|_| {
        HardwareDerivationError::InvalidDescriptor("Ledger EIP-712 string is too long")
    })?);
    output.extend_from_slice(value);
    Ok(())
}

fn ledger_len_prefixed_stringless_payload(value: &str) -> Result<Vec<u8>, HardwareDerivationError> {
    if value.len() > u8::MAX as usize {
        return Err(HardwareDerivationError::InvalidDescriptor(
            "Ledger EIP-712 name is too long",
        ));
    }
    Ok(value.as_bytes().to_vec())
}

fn u256_to_ledger(value: U256) -> Vec<u8> {
    let bytes = value.to_be_bytes::<32>();
    bytes[value.leading_zeros() / 8..].to_vec()
}

fn i256_to_ledger(value: &alloy::primitives::I256) -> Vec<u8> {
    let bytes = value.to_be_bytes::<32>();
    let sign_byte = if value.is_negative() { 0xff } else { 0x00 };
    let mut start = 0;
    while start < bytes.len()
        && bytes[start] == sign_byte
        && bytes
            .get(start + 1)
            .is_some_and(|next| (*next & 0x80) == (sign_byte & 0x80))
    {
        start += 1;
    }
    bytes[start..].to_vec()
}

fn ledger_signature_from_response(
    data: &[u8],
    _operation: &'static str,
) -> Result<Signature, HardwareDerivationError> {
    if data.len() != 65 {
        return Err(HardwareDerivationError::UnexpectedResponseLength {
            got: data.len(),
            expected: 65,
        });
    }
    let parity = normalize_v(u64::from(data[0])).ok_or(
        HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger signature has invalid recovery id",
        ),
    )?;
    Ok(Signature::from_bytes_and_parity(&data[1..], parity))
}

const fn ledger_hid_device_matches(vendor_id: u16, usage_page: u16) -> bool {
    if vendor_id != LEDGER_VID {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let _ = usage_page;
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        usage_page == LEDGER_USAGE_PAGE
    }
}

async fn ledger_signing_exchange(
    command: &APDUCommand,
    control: Option<&crate::dapp_request::DappRequestControl>,
    operation: &'static str,
) -> Result<APDUAnswer, HardwareDerivationError> {
    let command = command.clone();
    ledger_signing_dispatch(
        control.cloned(),
        move || {
            let api = HidApi::new()
                .map_err(NativeTransportError::Hid)
                .map_err(LedgerError::from)
                .map_err(|error| ledger_exchange_error(error, operation))?;
            let device = first_ledger(&api)
                .map_err(LedgerError::from)
                .map_err(|error| ledger_exchange_error(error, operation))?;
            Ok((api, device))
        },
        move |(_api, device)| {
            let data = ledger_write_read_apdu(&device, &command)
                .map_err(LedgerError::from)
                .map_err(|error| ledger_exchange_error(error, operation))?;
            APDUAnswer::from_answer(data).map_err(|error| ledger_exchange_error(error, operation))
        },
    )
    .await
}

// Admission belongs inside blocking dispatch, after opening the device. Both queues can wait.
async fn ledger_signing_dispatch<Device, Response>(
    control: Option<crate::dapp_request::DappRequestControl>,
    open: impl FnOnce() -> Result<Device, HardwareDerivationError> + Send + 'static,
    exchange: impl FnOnce(Device) -> Result<Response, HardwareDerivationError> + Send + 'static,
) -> Result<Response, HardwareDerivationError>
where
    Response: Send + 'static,
{
    let _guard = LEDGER_IO_LOCK.lock().await;
    task::spawn_blocking(move || {
        let device = open()?;
        if let Some(control) = control {
            control.ensure_current()?;
        }
        exchange(device)
    })
    .await
    .map_err(|_| HardwareDerivationError::Ledger(LedgerError::BackendGone))?
}

async fn ledger_exchange(command: &APDUCommand) -> Result<APDUAnswer, LedgerError> {
    let _guard = LEDGER_IO_LOCK.lock().await;
    let command = command.clone();
    task::spawn_blocking(move || ledger_exchange_blocking(&command))
        .await
        .map_err(|_| LedgerError::BackendGone)?
}

fn ledger_exchange_blocking(command: &APDUCommand) -> Result<APDUAnswer, LedgerError> {
    let api = HidApi::new().map_err(NativeTransportError::Hid)?;
    let device = first_ledger(&api)?;
    let data = ledger_write_read_apdu(&device, command)?;
    APDUAnswer::from_answer(data)
}

fn first_ledger(api: &HidApi) -> Result<HidDevice, NativeTransportError> {
    let device = api
        .device_list()
        .find(|device| ledger_hid_device_matches(device.vendor_id(), device.usage_page()))
        .ok_or(NativeTransportError::DeviceNotFound)?;
    open_ledger_device(api, device)
}

fn open_ledger_device(
    api: &HidApi,
    device: &DeviceInfo,
) -> Result<HidDevice, NativeTransportError> {
    let device = device
        .open_device(api)
        .map_err(NativeTransportError::CantOpen)?;
    let _ = device.set_blocking_mode(true);
    Ok(device)
}

fn ledger_write_read_apdu(
    device: &HidDevice,
    command: &APDUCommand,
) -> Result<Vec<u8>, NativeTransportError> {
    ledger_write_apdu(device, &command.serialize())?;
    ledger_read_response_apdu(device)
}

fn ledger_write_apdu(device: &HidDevice, apdu_command: &[u8]) -> Result<(), NativeTransportError> {
    let command_length = apdu_command.len();
    let mut in_data = Vec::with_capacity(command_length + 2);
    in_data.push(((command_length >> 8) & 0xff) as u8);
    in_data.push((command_length & 0xff) as u8);
    in_data.extend_from_slice(apdu_command);

    let mut buffer = [0u8; LEDGER_PACKET_WRITE_SIZE];
    buffer[1] = ((LEDGER_CHANNEL >> 8) & 0xff) as u8;
    buffer[2] = (LEDGER_CHANNEL & 0xff) as u8;
    buffer[3] = 0x05;

    for (sequence_idx, chunk) in in_data.chunks(LEDGER_PACKET_WRITE_SIZE - 6).enumerate() {
        buffer[4] = ((sequence_idx >> 8) & 0xff) as u8;
        buffer[5] = (sequence_idx & 0xff) as u8;
        buffer[6..6 + chunk.len()].copy_from_slice(chunk);

        let written = device.write(&buffer).map_err(NativeTransportError::Hid)?;
        if written < buffer.len() {
            return Err(NativeTransportError::Comm(
                "USB write error. Could not send whole message",
            ));
        }
    }
    Ok(())
}

fn ledger_read_response_apdu(device: &HidDevice) -> Result<Vec<u8>, NativeTransportError> {
    let mut response_buffer = [0u8; LEDGER_PACKET_READ_SIZE];
    let mut sequence_idx = 0u16;
    let mut expected_response_len = 0usize;
    let mut offset = 0usize;
    let mut answer_buf = vec![];

    loop {
        let read = device
            .read_timeout(&mut response_buffer, LEDGER_TIMEOUT_MS)
            .map_err(NativeTransportError::Hid)?;
        if (sequence_idx == 0 && read < 7) || read < 5 {
            return Err(NativeTransportError::Comm("Read error. Incomplete header"));
        }

        let mut cursor = Cursor::new(&response_buffer[..read]);
        let (_, _, response_sequence_idx) = ledger_read_response_header(&mut cursor)?;
        if response_sequence_idx != sequence_idx {
            return Err(NativeTransportError::SequenceMismatch {
                got: response_sequence_idx,
                expected: sequence_idx,
            });
        }

        if sequence_idx == 0 {
            expected_response_len = ledger_read_u16_be(&mut cursor)? as usize;
        }

        let cursor_position = usize::try_from(cursor.position()).map_err(|_| {
            NativeTransportError::Comm("Read error. Invalid response cursor position")
        })?;
        let remaining_in_buf = read.saturating_sub(cursor_position);
        let missing = expected_response_len.saturating_sub(offset);
        let chunk_len = remaining_in_buf.min(missing);
        let chunk_end = cursor_position + chunk_len;
        answer_buf.extend_from_slice(&response_buffer[cursor_position..chunk_end]);
        offset += chunk_len;

        if offset >= expected_response_len {
            return Ok(answer_buf);
        }
        sequence_idx = sequence_idx
            .checked_add(1)
            .ok_or(NativeTransportError::Comm(
                "Read error. Response sequence overflow",
            ))?;
    }
}

fn ledger_read_response_header(
    cursor: &mut Cursor<&[u8]>,
) -> Result<(u16, u8, u16), NativeTransportError> {
    let channel = ledger_read_u16_be(cursor)?;
    let tag = ledger_read_u8(cursor)?;
    let sequence_idx = ledger_read_u16_be(cursor)?;
    Ok((channel, tag, sequence_idx))
}

fn ledger_read_u16_be(cursor: &mut Cursor<&[u8]>) -> Result<u16, NativeTransportError> {
    let hi = u16::from(ledger_read_u8(cursor)?);
    let lo = u16::from(ledger_read_u8(cursor)?);
    Ok((hi << 8) | lo)
}

fn ledger_read_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8, NativeTransportError> {
    let position = usize::try_from(cursor.position())
        .map_err(|_| NativeTransportError::Comm("Read error. Invalid response cursor position"))?;
    let byte = cursor
        .get_ref()
        .get(position)
        .copied()
        .ok_or(NativeTransportError::Comm("Read error. Incomplete header"))?;
    cursor.set_position(cursor.position() + 1);
    Ok(byte)
}

fn ledger_exchange_error(error: LedgerError, operation: &'static str) -> HardwareDerivationError {
    match error {
        LedgerError::BadRetcode(status) => ledger_status_error(operation, status as u16),
        error => HardwareDerivationError::Ledger(error),
    }
}

fn ledger_ensure_success(
    answer: &APDUAnswer,
    operation: &'static str,
) -> Result<(), HardwareDerivationError> {
    if answer.is_success() {
        Ok(())
    } else {
        Err(ledger_status_error(operation, answer.retcode()))
    }
}

fn ledger_response_data<'a>(
    answer: &'a APDUAnswer,
    operation: &'static str,
) -> Result<&'a [u8], HardwareDerivationError> {
    ledger_ensure_success(answer, operation)?;
    answer
        .data()
        .ok_or(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger success response has no data",
        ))
}

const fn ledger_status_error(operation: &'static str, status: u16) -> HardwareDerivationError {
    HardwareDerivationError::LedgerStatus {
        operation,
        status,
        message: ledger_status_message(status),
    }
}

const fn ledger_status_message(status: u16) -> &'static str {
    match status {
        0x6511 | 0x6a15 | 0x6d00 | 0x6e00 => "Open the Ethereum app on your Ledger, then retry.",
        0x6804 | 0x6b0c => "Enter your PIN, then retry.",
        0x6982 => "The request was rejected on your Ledger.",
        0x6985 => {
            "The request was rejected or the Ledger is not ready. Approve on device or retry."
        }
        0x6a80 | 0x6b00 => {
            "The Ledger rejected the request data. Confirm the account path and retry."
        }
        _ => {
            "Ledger returned an unexpected status. Open the Ethereum app on your Ledger and retry."
        }
    }
}

const fn ledger_signing_operation(ins: u8) -> &'static str {
    match ins {
        0x04 => "sign transaction",
        0x08 => "sign message",
        _ => "sign payload",
    }
}

const fn ledger_address_display_p1(display_and_confirm: bool) -> u8 {
    if display_and_confirm { 0x01 } else { 0x00 }
}

fn ledger_path_payload(path: &[u32]) -> Result<Vec<u8>, HardwareDerivationError> {
    let mut data = Vec::with_capacity(1 + path.len() * 4);
    data.push(u8::try_from(path.len()).map_err(|_| {
        HardwareDerivationError::InvalidDescriptor(
            "Ledger EIP-1024 path contains too many segments",
        )
    })?);
    for index in path {
        data.extend_from_slice(&index.to_be_bytes());
    }
    Ok(data)
}

fn ledger_address_from_response(data: &[u8]) -> Result<String, HardwareDerivationError> {
    let Some((&public_key_len, rest)) = data.split_first() else {
        return Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger address response is missing public key length",
        ));
    };
    let address_len_offset = usize::from(public_key_len);
    if rest.len() <= address_len_offset {
        return Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger address response is missing address length",
        ));
    }
    let address_len = usize::from(rest[address_len_offset]);
    let address_start = address_len_offset + 1;
    let address_end = address_start + address_len;
    if rest.len() < address_end {
        return Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger address response is truncated",
        ));
    }
    let address = std::str::from_utf8(&rest[address_start..address_end]).map_err(|_| {
        HardwareDerivationError::UnexpectedHardwareResponse("Ledger address response is not UTF-8")
    })?;
    if hex::decode(address).map_or(true, |bytes| bytes.len() != 20) {
        return Err(HardwareDerivationError::UnexpectedHardwareResponse(
            "Ledger address response is not an EVM address",
        ));
    }
    Ok(address.to_owned())
}

#[async_trait(?Send)]
impl HardwareDerivationClient for LedgerHardwareDerivationClient {
    async fn derive_hardware_output(
        &mut self,
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<HardwareOperationOutput, HardwareDerivationError> {
        descriptor.validate()?;
        if descriptor.method != HardwareDerivationMethod::LedgerEip1024V1 {
            return Err(HardwareDerivationError::InvalidDescriptor(
                "Ledger client requires a Ledger EIP-1024 descriptor",
            ));
        }
        self.eip1024_shared_secret(&descriptor.path, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn signing_dispatch_rechecks_after_queueing_and_device_preparation() {
        use crate::{RpcBrokerError, dapp_request::DappRequestControl};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;
        use tokio::time::{Instant, timeout};

        let writes = Arc::new(AtomicUsize::new(0));
        let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
        let lock = LEDGER_IO_LOCK.lock().await;
        let queued = ledger_signing_dispatch(Some(control.clone()), || Ok(()), {
            let writes = writes.clone();
            move |()| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        tokio::pin!(queued);
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        control.invalidate(&RpcBrokerError::OriginRejected);
        drop(lock);
        assert!(matches!(
            timeout(Duration::from_secs(2), queued).await.unwrap(),
            Err(HardwareDerivationError::RequestAuthority(
                RpcBrokerError::OriginRejected
            ))
        ));

        let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let preparing = tokio::spawn(ledger_signing_dispatch(
            Some(control.clone()),
            move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            },
            {
                let writes = writes.clone();
                move |()| {
                    writes.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        ));
        timeout(Duration::from_secs(2), entered_rx)
            .await
            .unwrap()
            .unwrap();
        control.invalidate(&RpcBrokerError::Shutdown);
        release_tx.send(()).unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), preparing)
                .await
                .unwrap()
                .unwrap(),
            Err(HardwareDerivationError::RequestAuthority(
                RpcBrokerError::Shutdown
            ))
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 0);

        // A later signing APDU or fallback attempt must re-enter admission after preparation.
        let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
        ledger_signing_dispatch(Some(control.clone()), || Ok(()), |()| Ok(()))
            .await
            .unwrap();
        control.invalidate(&RpcBrokerError::OriginRejected);
        let result = ledger_signing_dispatch(Some(control), || Ok(()), {
            let writes = writes.clone();
            move |()| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await;
        let error = result.unwrap_err();
        assert_eq!(
            classify_ledger_eip712_clear_failure(&error, &ledger_sign_apdu(), false),
            LedgerEip712ClearFailureKind::Other
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    fn answer_with_status(status: u16) -> APDUAnswer {
        APDUAnswer::from_answer(status.to_be_bytes().to_vec()).expect("status answer")
    }

    #[test]
    fn ledger_hid_preflight_filter_matches_coins_ledger_filter() {
        #[cfg(not(target_os = "linux"))]
        {
            assert!(ledger_hid_device_matches(0x2c97, 0xffa0));
            assert!(!ledger_hid_device_matches(0x2c97, 0x0001));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(ledger_hid_device_matches(0x2c97, 0x0001));
        }
        assert!(!ledger_hid_device_matches(0x1234, 0xffa0));
    }

    #[test]
    fn ledger_connect_device_not_found_preserves_retry_guidance() {
        let error = HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE);

        assert!(matches!(
            error,
            HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE)
        ));
        assert!(error.to_string().contains("enter your PIN"));
        assert!(error.to_string().contains("open the Ethereum app"));
    }

    #[test]
    fn ledger_app_closed_status_points_to_ethereum_app() {
        let error = ledger_response_data(&answer_with_status(0x6511), "get Ethereum address")
            .expect_err("app closed status should fail");

        assert!(matches!(
            error,
            HardwareDerivationError::LedgerStatus {
                operation: "get Ethereum address",
                status: 0x6511,
                ..
            }
        ));
        let message = error.to_string();
        assert!(message.contains("0x6511"));
        assert!(message.contains("Open the Ethereum app on your Ledger"));
    }

    #[test]
    fn ledger_known_bad_retcode_points_to_ethereum_app() {
        let error = ledger_exchange_error(
            LedgerError::BadRetcode(coins_ledger::common::APDUResponseCodes::InsNotSupported),
            "get Ethereum app version",
        );

        assert!(matches!(
            error,
            HardwareDerivationError::LedgerStatus {
                operation: "get Ethereum app version",
                status: 0x6d00,
                ..
            }
        ));
        assert!(
            error
                .to_string()
                .contains("Open the Ethereum app on your Ledger")
        );
    }

    #[test]
    fn ledger_locked_status_points_to_pin_entry() {
        let error = ledger_response_data(&answer_with_status(0x6b0c), "get Ethereum address")
            .expect_err("locked status should fail");

        assert!(matches!(
            error,
            HardwareDerivationError::LedgerStatus { status: 0x6b0c, .. }
        ));
        assert!(error.to_string().contains("Enter your PIN"));
    }

    #[test]
    fn ledger_address_confirmation_sets_display_flag() {
        assert_eq!(ledger_address_display_p1(false), 0x00);
        assert_eq!(ledger_address_display_p1(true), 0x01);
    }

    #[test]
    fn ledger_typed_data_classification_is_conservative() {
        let clear = LedgerDeviceInfo {
            model: LedgerDeviceModel::NanoX,
            ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
        };
        let fallback = LedgerDeviceInfo {
            model: LedgerDeviceModel::NanoS,
            ethereum_app_version: HardwareAppVersion::new(1, 5, 0),
        };
        let unknown_model = LedgerDeviceInfo {
            model: LedgerDeviceModel::Unknown,
            ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
        };
        let unsupported = LedgerDeviceInfo {
            model: LedgerDeviceModel::NanoX,
            ethereum_app_version: HardwareAppVersion::new(1, 4, 9),
        };

        assert_eq!(
            classify_ledger_typed_data_signing_mode(&clear),
            HardwareTypedDataSigningMode::ClearSign
        );
        assert_eq!(
            classify_ledger_typed_data_signing_mode(&fallback),
            HardwareTypedDataSigningMode::Eip712HashFallback
        );
        assert_eq!(
            classify_ledger_typed_data_signing_mode(&unknown_model),
            HardwareTypedDataSigningMode::Eip712HashFallback
        );
        assert_eq!(
            classify_ledger_typed_data_signing_mode(&unsupported),
            HardwareTypedDataSigningMode::Unsupported
        );
    }

    fn ledger_clear_info() -> LedgerDeviceInfo {
        LedgerDeviceInfo {
            model: LedgerDeviceModel::NanoX,
            ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
        }
    }

    fn ledger_no_hash_fallback_info() -> LedgerDeviceInfo {
        LedgerDeviceInfo {
            model: LedgerDeviceModel::NanoX,
            ethereum_app_version: HardwareAppVersion::new(1, 4, 9),
        }
    }

    fn ledger_definition_apdu() -> LedgerEip712Apdu {
        LedgerEip712Apdu {
            ins: 0x1a,
            p1: 0x00,
            p2: 0x00,
            data: b"Message".to_vec(),
        }
    }

    fn ledger_sign_apdu() -> LedgerEip712Apdu {
        LedgerEip712Apdu {
            ins: 0x0c,
            p1: 0x00,
            p2: 0x01,
            data: vec![0],
        }
    }

    #[test]
    fn ledger_clear_failure_downgrades_only_for_pre_confirmation_capability_rejection() {
        let error = ledger_status_error("sign EIP-712 typed data", 0x6d00);
        let failed_apdu = ledger_definition_apdu();

        assert_eq!(
            classify_ledger_eip712_clear_failure(&error, &failed_apdu, false),
            LedgerEip712ClearFailureKind::CapabilityUnsupportedBeforeConfirmation
        );
        assert_eq!(
            ledger_eip712_clear_failure_downgrade_mode(
                &error,
                &failed_apdu,
                false,
                ledger_clear_info()
            ),
            Some(HardwareTypedDataSigningMode::Eip712HashFallback)
        );
        assert_eq!(
            ledger_eip712_clear_failure_downgrade_mode(
                &error,
                &failed_apdu,
                false,
                ledger_no_hash_fallback_info()
            ),
            None
        );
    }

    #[test]
    fn ledger_clear_failure_does_not_downgrade_payload_specific_rejection() {
        let error = ledger_status_error("sign EIP-712 typed data", 0x6a80);
        let failed_apdu = ledger_definition_apdu();

        assert_eq!(
            classify_ledger_eip712_clear_failure(&error, &failed_apdu, false),
            LedgerEip712ClearFailureKind::PayloadUnsupported
        );
        assert_eq!(
            ledger_eip712_clear_failure_downgrade_mode(
                &error,
                &failed_apdu,
                false,
                ledger_clear_info()
            ),
            None
        );
    }

    #[test]
    fn ledger_clear_failure_does_not_downgrade_signing_or_user_rejection() {
        let unsupported_error = ledger_status_error("sign EIP-712 typed data", 0x6d00);
        let rejected_error = ledger_status_error("sign EIP-712 typed data", 0x6985);
        let sign_apdu = ledger_sign_apdu();
        let definition_apdu = ledger_definition_apdu();

        assert_eq!(
            classify_ledger_eip712_clear_failure(&unsupported_error, &sign_apdu, false),
            LedgerEip712ClearFailureKind::Other
        );
        assert_eq!(
            classify_ledger_eip712_clear_failure(&unsupported_error, &definition_apdu, true),
            LedgerEip712ClearFailureKind::Other
        );
        assert_eq!(
            classify_ledger_eip712_clear_failure(&rejected_error, &definition_apdu, false),
            LedgerEip712ClearFailureKind::UserRejected
        );
        assert_eq!(
            ledger_eip712_clear_failure_downgrade_mode(
                &rejected_error,
                &definition_apdu,
                false,
                ledger_clear_info()
            ),
            None
        );
    }

    #[test]
    fn ledger_device_model_uses_product_string_before_product_id() {
        assert_eq!(
            ledger_device_model(0xffff, Some("Ledger Nano X")),
            LedgerDeviceModel::NanoX
        );
        assert_eq!(
            ledger_device_model(0x0005, None),
            LedgerDeviceModel::NanoSPlus
        );
        assert_eq!(
            ledger_device_model(0xffff, None),
            LedgerDeviceModel::Unknown
        );
    }

    fn typed_data_model() -> HardwareEip712Model {
        HardwareEip712Model::from_walletconnect_typed_data_json(json!({
            "types": {
                "EIP712Domain": [
                    { "name": "name", "type": "string" },
                    { "name": "chainId", "type": "uint256" }
                ],
                "Person": [
                    { "name": "wallet", "type": "address" },
                    { "name": "name", "type": "string" }
                ],
                "Message": [
                    { "name": "count", "type": "uint256" },
                    { "name": "people", "type": "Person[]" },
                    { "name": "tags", "type": "string[2]" },
                    { "name": "payload", "type": "bytes" },
                    { "name": "digest", "type": "bytes32" }
                ]
            },
            "primaryType": "Message",
            "domain": {
                "name": "RailOxide",
                "chainId": 1
            },
            "message": {
                "count": 128,
                "people": [
                    {
                        "wallet": "0x1111111111111111111111111111111111111111",
                        "name": "Alice"
                    }
                ],
                "tags": ["permit", "test"],
                "payload": "0x010203",
                "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }))
        .expect("typed-data model")
    }

    fn reordered_domain_typed_data_model() -> HardwareEip712Model {
        HardwareEip712Model::from_walletconnect_typed_data_json(json!({
            "types": {
                "EIP712Domain": [
                    { "name": "chainId", "type": "uint256" },
                    { "name": "name", "type": "string" }
                ],
                "Message": [
                    { "name": "contents", "type": "string" }
                ]
            },
            "primaryType": "Message",
            "domain": {
                "name": "RailOxide",
                "chainId": 1
            },
            "message": {
                "contents": "hello"
            }
        }))
        .expect("typed-data model")
    }

    fn long_string_typed_data_model() -> HardwareEip712Model {
        HardwareEip712Model::from_walletconnect_typed_data_json(json!({
            "types": {
                "EIP712Domain": [],
                "Message": [
                    { "name": "contents", "type": "string" }
                ]
            },
            "primaryType": "Message",
            "domain": {},
            "message": {
                "contents": "x".repeat(300)
            }
        }))
        .expect("long string typed-data model")
    }

    fn ledger_descriptor() -> HardwarePublicAccountDescriptor {
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Ledger, 0, 0)
            .expect("ledger descriptor")
    }

    #[test]
    fn ledger_eip712_definition_apdus_encode_doc_type_descriptors() {
        let model = typed_data_model();
        let definitions = ledger_eip712_definition_apdus(&model).expect("definitions");

        assert_eq!(definitions[0].ins, 0x1a);
        assert_eq!(definitions[0].p2, 0x00);
        assert_eq!(definitions[0].data, b"EIP712Domain");

        let count = definitions
            .iter()
            .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x05count"))
            .expect("count field definition");
        assert_eq!(count.data, [0x42, 32, 5, b'c', b'o', b'u', b'n', b't']);

        let people = definitions
            .iter()
            .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x06people"))
            .expect("people field definition");
        assert_eq!(
            people.data,
            [
                0x80, 6, b'P', b'e', b'r', b's', b'o', b'n', 1, 0, 6, b'p', b'e', b'o', b'p', b'l',
                b'e'
            ]
        );

        let tags = definitions
            .iter()
            .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x04tags"))
            .expect("tags field definition");
        assert_eq!(tags.data, [0x85, 1, 1, 2, 4, b't', b'a', b'g', b's']);
    }

    #[test]
    fn ledger_eip712_implementation_apdus_traverse_values_in_order() {
        let model = typed_data_model();
        let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");

        assert_eq!(implementations[0].ins, 0x1c);
        assert_eq!(implementations[0].p1, 0x00);
        assert_eq!(implementations[0].p2, 0x00);
        assert_eq!(implementations[0].data, b"EIP712Domain");
        assert_eq!(implementations[1].p1, 0x00);
        assert_eq!(implementations[1].p2, 0xff);
        assert_eq!(implementations[1].data, b"\x00\x09RailOxide");
        assert_eq!(implementations[2].p1, 0x00);
        assert_eq!(implementations[2].p2, 0xff);
        assert_eq!(implementations[2].data, b"\x00\x01\x01");

        let message_root = implementations
            .iter()
            .position(|apdu| apdu.p2 == 0x00 && apdu.data == b"Message")
            .expect("message root");
        assert_eq!(implementations[message_root + 1].data, b"\x00\x01\x80");
        assert_eq!(implementations[message_root + 2].p1, 0x00);
        assert_eq!(implementations[message_root + 2].p2, 0x0f);
        assert_eq!(implementations[message_root + 2].data, [1]);
        assert!(implementations.iter().all(|apdu| apdu.p1 == 0x00));
    }

    #[test]
    fn ledger_eip712_domain_values_follow_canonical_hash_order() {
        let model = reordered_domain_typed_data_model();
        let definitions = ledger_eip712_definition_apdus(&model).expect("definitions");
        let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");

        assert_eq!(definitions[0].data, b"EIP712Domain");
        assert!(definitions[1].data.ends_with(b"\x04name"));
        assert!(definitions[2].data.ends_with(b"\x07chainId"));
        assert_eq!(implementations[0].data, b"EIP712Domain");
        assert_eq!(implementations[1].data, b"\x00\x09RailOxide");
        assert_eq!(implementations[2].data, b"\x00\x01\x01");
    }

    #[test]
    fn ledger_eip712_field_values_are_chunked_without_truncation() {
        let model = long_string_typed_data_model();
        let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");
        let chunks: Vec<_> = implementations
            .iter()
            .filter(|apdu| apdu.p2 == 0xff)
            .collect();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].p1, 0x01);
        assert_eq!(chunks[0].data.len(), 255);
        assert_eq!(chunks[0].data[..2], [0x01, 0x2c]);
        assert_eq!(chunks[1].p1, 0x00);
        assert_eq!(chunks[1].data.len(), 47);
    }

    #[test]
    fn ledger_eip712_hash_fallback_payload_uses_local_hashes() {
        let model = typed_data_model();
        let descriptor = ledger_descriptor();

        let apdu = ledger_eip712_hash_signing_apdu(&descriptor, &model).expect("hash apdu");

        let path = ledger_path_payload(&descriptor.path).expect("path payload");
        assert_eq!(apdu.ins, 0x0c);
        assert_eq!(apdu.p1, 0x00);
        assert_eq!(apdu.p2, 0x00);
        assert_eq!(apdu.data.len(), path.len() + 64);
        assert_eq!(&apdu.data[..path.len()], path.as_slice());
        assert_eq!(
            &apdu.data[path.len()..path.len() + 32],
            model.domain_separator_hash().as_slice()
        );
        assert_eq!(
            &apdu.data[path.len() + 32..],
            model.message_hash().expect("message hash").as_slice()
        );
    }

    #[test]
    fn ledger_eip712_clear_sequence_sends_definitions_implementations_then_sign() {
        let model = typed_data_model();
        let descriptor = ledger_descriptor();

        let apdus = ledger_eip712_clear_signing_apdus(&descriptor, &model).expect("clear apdus");

        assert!(apdus.iter().take_while(|apdu| apdu.ins == 0x1a).count() > 0);
        let first_implementation = apdus
            .iter()
            .position(|apdu| apdu.ins == 0x1c)
            .expect("implementation APDU");
        let final_apdu = apdus.last().expect("final sign APDU");
        assert!(
            apdus[..first_implementation]
                .iter()
                .all(|apdu| apdu.ins == 0x1a)
        );
        assert!(
            apdus[first_implementation..apdus.len() - 1]
                .iter()
                .all(|apdu| apdu.ins == 0x1c)
        );
        assert_eq!(final_apdu.ins, 0x0c);
        assert_eq!(final_apdu.p2, 0x01);
        assert_eq!(
            final_apdu.data,
            ledger_path_payload(&descriptor.path).expect("path")
        );
    }
}
