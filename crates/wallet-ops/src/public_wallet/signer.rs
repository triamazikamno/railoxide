use std::sync::Mutex;

use alloy::consensus::SignableTransaction;
use alloy::eips::Encodable2718;
use alloy::network::NetworkTransactionBuilder;
use alloy::primitives::{Address, Signature, keccak256};
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, WrapErr, eyre};
use zeroize::Zeroizing;

#[cfg(feature = "hardware")]
use crate::hardware::{DEFAULT_HARDWARE_DERIVATION_PATH, parse_bip32_path};
use crate::hardware::{HardwarePublicAccountDescriptor, HardwareTypedDataSigningMode};
use crate::hardware_typed_data::HardwareEip712Model;
use crate::signer::{EvmMessageSigner, EvmTransactionSigner, SoftwareEvmSigner};
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, HardwareProfileSession, ProtectedSoftwareSeedSession,
};

use super::types::{
    HardwareTrezorPinMatrixProvider, WalletConnectHardwareTypedDataHashFallbackConfirmationRequired,
};

pub(crate) enum VaultedPublicSigner {
    Software(SoftwareEvmSigner),
    Hardware(HardwarePublicEvmSigner),
}

pub(crate) struct HardwarePublicEvmSigner {
    pub(super) address: Address,
    pub(super) descriptor: HardwarePublicAccountDescriptor,
    pub(super) hardware_session: Mutex<HardwareProfileSession>,
    pub(super) trezor_app_passphrase: Mutex<Option<Zeroizing<String>>>,
    pub(super) trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
}

impl VaultedPublicSigner {
    pub(crate) fn address(&self) -> Address {
        match self {
            Self::Software(signer) => signer.address(),
            Self::Hardware(signer) => signer.address,
        }
    }

    pub(crate) const fn requires_device_approval(&self) -> bool {
        matches!(self, Self::Hardware(_))
    }

    pub(crate) async fn sign_transaction_request(
        &self,
        tx_req: TransactionRequest,
        label: &str,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Software(signer) => {
                let wallet = signer.ethereum_wallet();
                Ok(tx_req
                    .build(&wallet)
                    .await
                    .wrap_err_with(|| format!("{label}: sign"))?
                    .encoded_2718())
            }
            Self::Hardware(signer) => signer.sign_transaction_request(tx_req, label).await,
        }
    }

    pub(crate) async fn derive_shield_private_key(&self) -> Result<Zeroizing<[u8; 32]>> {
        match self {
            Self::Software(signer) => Ok(Zeroizing::new(signer.derive_shield_private_key()?)),
            Self::Hardware(signer) => signer.derive_shield_private_key().await,
        }
    }

    pub(super) async fn sign_personal_message(&self, message: &[u8]) -> Result<Signature> {
        match self {
            Self::Software(signer) => signer.sign_personal_message(message),
            Self::Hardware(signer) => signer.sign_message(message).await,
        }
    }

    pub(super) async fn typed_data_signing_mode(
        &self,
    ) -> Result<Option<HardwareTypedDataSigningMode>> {
        match self {
            Self::Software(_) => Ok(None),
            Self::Hardware(signer) => signer.typed_data_signing_mode().await.map(Some),
        }
    }

    pub(super) async fn sign_typed_data_v4(
        &self,
        typed_data: &HardwareEip712Model,
        hardware_typed_data_mode: Option<HardwareTypedDataSigningMode>,
        hash_fallback_confirmed: bool,
    ) -> Result<Signature> {
        match self {
            Self::Software(signer) => signer.sign_typed_data_v4(typed_data.typed_data()),
            Self::Hardware(signer) => {
                signer
                    .sign_typed_data_v4(
                        typed_data,
                        hardware_typed_data_mode,
                        hash_fallback_confirmed,
                    )
                    .await
            }
        }
    }

    pub(crate) fn refreshed_hardware_session(&self) -> Result<Option<HardwareProfileSession>> {
        match self {
            Self::Software(_) => Ok(None),
            Self::Hardware(signer) => signer.hardware_session().map(Some),
        }
    }
}

impl HardwarePublicEvmSigner {
    async fn sign_transaction_request(
        &self,
        tx_req: TransactionRequest,
        label: &str,
    ) -> Result<Vec<u8>> {
        let tx = tx_req
            .build_consensus_tx()
            .map_err(|error| eyre!(error.error))
            .wrap_err_with(|| format!("{label}: build hardware transaction"))?;
        let signature = self
            .sign_transaction(&tx)
            .await
            .wrap_err_with(|| format!("{label}: hardware sign"))?;
        let signing_hash = tx.signature_hash();
        let recovered = signature
            .recover_address_from_prehash(&signing_hash)
            .wrap_err_with(|| format!("{label}: recover hardware signature"))?;
        if recovered != self.address {
            return Err(eyre!(
                "hardware public signer address mismatch: expected {}, got {}",
                self.address,
                recovered
            ));
        }
        Ok(tx.into_envelope(signature).encoded_2718())
    }

    async fn derive_shield_private_key(&self) -> Result<Zeroizing<[u8; 32]>> {
        const SHIELD_MESSAGE: &[u8] = b"RAILGUN_SHIELD";
        let signature = self
            .sign_message(SHIELD_MESSAGE)
            .await
            .wrap_err("hardware sign shield key message")?;
        let recovered = signature
            .recover_address_from_msg(SHIELD_MESSAGE)
            .wrap_err("recover hardware shield key signature")?;
        if recovered != self.address {
            return Err(eyre!(
                "hardware public signer address mismatch: expected {}, got {}",
                self.address,
                recovered
            ));
        }
        let signature_bytes = Zeroizing::new(signature.as_bytes());
        Ok(Zeroizing::new(keccak256(*signature_bytes).0))
    }

    async fn sign_transaction(&self, tx: &dyn SignableTransaction<Signature>) -> Result<Signature> {
        let hardware_session = self.hardware_session()?;
        let (signature, trezor_session_id) = sign_hardware_public_transaction(
            &self.descriptor,
            &hardware_session,
            self.take_trezor_app_passphrase(),
            self.trezor_pin_matrix_provider(),
            self.address,
            tx,
        )
        .await?;
        self.replace_trezor_session_id_if_trezor(trezor_session_id)?;
        Ok(signature)
    }

    async fn sign_message(&self, message: &[u8]) -> Result<Signature> {
        let hardware_session = self.hardware_session()?;
        let (signature, trezor_session_id) = sign_hardware_public_message(
            &self.descriptor,
            &hardware_session,
            self.take_trezor_app_passphrase(),
            self.trezor_pin_matrix_provider(),
            self.address,
            message,
        )
        .await?;
        self.replace_trezor_session_id_if_trezor(trezor_session_id)?;
        Ok(signature)
    }

    pub(super) async fn sign_typed_data_v4(
        &self,
        typed_data: &HardwareEip712Model,
        hardware_typed_data_mode: Option<HardwareTypedDataSigningMode>,
        hash_fallback_confirmed: bool,
    ) -> Result<Signature> {
        let mut mode = match hardware_typed_data_mode {
            Some(mode) => mode,
            None => self.typed_data_signing_mode().await?,
        };
        loop {
            if !mode.is_supported() {
                return Err(eyre!(
                    "WalletConnect EIP-712 typed-data signing is unsupported for this hardware Public account session"
                ));
            }
            if mode.requires_hash_fallback_warning() && !hash_fallback_confirmed {
                return Err(
                    WalletConnectHardwareTypedDataHashFallbackConfirmationRequired::new(Some(
                        self.hardware_session()?,
                    ))
                    .into(),
                );
            }
            let hardware_session = self.hardware_session()?;
            let outcome = sign_hardware_public_typed_data(
                &self.descriptor,
                &hardware_session,
                self.take_trezor_app_passphrase(),
                self.trezor_pin_matrix_provider(),
                self.address,
                typed_data,
                mode,
            )
            .await?;
            let HardwareTypedDataSignOutcome::Signed {
                signature,
                trezor_session_id,
            } = outcome
            else {
                self.downgrade_typed_data_signing_mode_to_hash_fallback()?;
                mode = HardwareTypedDataSigningMode::Eip712HashFallback;
                continue;
            };
            self.replace_trezor_session_id_if_trezor(trezor_session_id)?;
            verify_hardware_typed_data_signature_address(self.address, &signature, typed_data)?;
            return Ok(signature);
        }
    }

    pub(super) async fn typed_data_signing_mode(&self) -> Result<HardwareTypedDataSigningMode> {
        if let Some(mode) = self
            .hardware_session()?
            .typed_data_signing_mode(&self.descriptor)
        {
            return Ok(mode);
        }
        let hardware_session = self.hardware_session()?;
        let (mode, trezor_session_id) = probe_hardware_public_typed_data_signing_mode(
            &self.descriptor,
            &hardware_session,
            self.take_trezor_app_passphrase(),
            self.trezor_pin_matrix_provider(),
            self.address,
        )
        .await?;

        let mut session = self
            .hardware_session
            .lock()
            .map_err(|_| eyre!("hardware public signer session lock poisoned"))?;
        if self.descriptor.device_kind == crate::hardware::HardwareDeviceKind::Trezor {
            session.trezor_session_id = trezor_session_id;
        }
        session.cache_typed_data_signing_mode(&self.descriptor, mode)?;
        Ok(mode)
    }

    pub(super) fn hardware_session(&self) -> Result<HardwareProfileSession> {
        let session = self
            .hardware_session
            .lock()
            .map_err(|_| eyre!("hardware public signer session lock poisoned"))?;
        Ok(session.clone())
    }

    #[cfg(feature = "hardware")]
    fn trezor_pin_matrix_provider(&self) -> Option<HardwareTrezorPinMatrixProvider> {
        self.trezor_pin_matrix_provider.clone()
    }

    #[cfg(not(feature = "hardware"))]
    const fn trezor_pin_matrix_provider(&self) -> Option<HardwareTrezorPinMatrixProvider> {
        self.trezor_pin_matrix_provider
    }

    pub(super) fn replace_trezor_session_id_if_trezor(
        &self,
        session_id: Option<Vec<u8>>,
    ) -> Result<()> {
        if self.descriptor.device_kind != crate::hardware::HardwareDeviceKind::Trezor {
            return Ok(());
        }
        let mut session = self
            .hardware_session
            .lock()
            .map_err(|_| eyre!("hardware public signer session lock poisoned"))?;
        session.replace_trezor_session_id_preserving_typed_data_signing_mode(
            &self.descriptor,
            session_id,
        )?;
        Ok(())
    }

    fn downgrade_typed_data_signing_mode_to_hash_fallback(&self) -> Result<()> {
        let mut session = self
            .hardware_session
            .lock()
            .map_err(|_| eyre!("hardware public signer session lock poisoned"))?;
        session.downgrade_typed_data_signing_mode_to_hash_fallback(&self.descriptor)?;
        Ok(())
    }

    pub(super) fn take_trezor_app_passphrase(&self) -> Option<Zeroizing<String>> {
        self.trezor_app_passphrase
            .lock()
            .ok()
            .and_then(|mut passphrase| passphrase.take())
    }
}

pub(super) fn verify_hardware_typed_data_signature_address(
    expected_address: Address,
    signature: &Signature,
    typed_data: &HardwareEip712Model,
) -> Result<()> {
    let signing_hash = typed_data.signing_hash();
    let recovered = signature
        .recover_address_from_prehash(&signing_hash)
        .wrap_err("recover hardware typed-data signature")?;
    if recovered != expected_address {
        return Err(eyre!(
            "hardware public signer address mismatch: expected {}, got {}",
            expected_address,
            recovered
        ));
    }
    Ok(())
}

#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
enum HardwareTypedDataSignOutcome {
    Signed {
        signature: Signature,
        trezor_session_id: Option<Vec<u8>>,
    },
    DowngradedToHashFallback,
}

#[cfg(feature = "hardware")]
async fn sign_hardware_public_typed_data(
    descriptor: &HardwarePublicAccountDescriptor,
    hardware_session: &HardwareProfileSession,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    expected_address: Address,
    typed_data: &HardwareEip712Model,
    mode: HardwareTypedDataSigningMode,
) -> Result<HardwareTypedDataSignOutcome> {
    match descriptor.device_kind {
        crate::hardware::HardwareDeviceKind::Ledger => {
            let client = crate::hardware::ledger::LedgerHardwareDerivationClient::connect()
                .await
                .wrap_err("connect Ledger for public typed-data signing")?;
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .await
                .wrap_err("verify Ledger hardware profile")?;
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .await
                .wrap_err("verify Ledger public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = match mode {
                HardwareTypedDataSigningMode::ClearSign => match client
                    .sign_typed_data_clear_or_downgrade(descriptor, typed_data)
                    .await
                {
                    Ok(crate::hardware::ledger::LedgerEip712ClearSigningOutcome::Signed(
                        signature,
                    )) => signature,
                    Ok(crate::hardware::ledger::LedgerEip712ClearSigningOutcome::Downgrade(
                        HardwareTypedDataSigningMode::Eip712HashFallback,
                    )) => return Ok(HardwareTypedDataSignOutcome::DowngradedToHashFallback),
                    Ok(crate::hardware::ledger::LedgerEip712ClearSigningOutcome::Downgrade(
                        HardwareTypedDataSigningMode::ClearSign
                        | HardwareTypedDataSigningMode::Unsupported,
                    )) => {
                        return Err(eyre!(
                            "WalletConnect EIP-712 typed-data signing cannot downgrade Ledger clear signing to a safe fallback"
                        ));
                    }
                    Err(error) => return Err(error).wrap_err("sign public typed data on Ledger"),
                },
                HardwareTypedDataSigningMode::Eip712HashFallback => client
                    .sign_typed_data_hash(descriptor, typed_data)
                    .await
                    .wrap_err("sign public typed-data hashes on Ledger")?,
                HardwareTypedDataSigningMode::Unsupported => {
                    return Err(eyre!(
                        "WalletConnect EIP-712 typed-data signing is unsupported for this Ledger session"
                    ));
                }
            };
            Ok(HardwareTypedDataSignOutcome::Signed {
                signature,
                trezor_session_id: None,
            })
        }
        crate::hardware::HardwareDeviceKind::Trezor => {
            let mut client =
                crate::hardware::trezor::TrezorHardwareDerivationClient::connect_with_session(
                    hardware_session.trezor_session_id.clone(),
                )
                .wrap_err("connect Trezor for public typed-data signing")?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .wrap_err("verify Trezor hardware profile")?;
            let trezor_session_id = active.trezor_session_id.clone();
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .wrap_err("verify Trezor public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = match mode {
                HardwareTypedDataSigningMode::ClearSign => client
                    .sign_typed_data_clear(descriptor, typed_data)
                    .wrap_err("sign public typed data on Trezor")?,
                HardwareTypedDataSigningMode::Eip712HashFallback => client
                    .sign_typed_data_hash(descriptor, typed_data)
                    .wrap_err("sign public typed-data hashes on Trezor")?,
                HardwareTypedDataSigningMode::Unsupported => {
                    return Err(eyre!(
                        "WalletConnect EIP-712 typed-data signing is unsupported for this Trezor session"
                    ));
                }
            };
            Ok(HardwareTypedDataSignOutcome::Signed {
                signature,
                trezor_session_id,
            })
        }
    }
}

#[cfg(not(feature = "hardware"))]
#[allow(clippy::unused_async)]
async fn sign_hardware_public_typed_data(
    _descriptor: &HardwarePublicAccountDescriptor,
    _hardware_session: &HardwareProfileSession,
    _trezor_app_passphrase: Option<Zeroizing<String>>,
    _trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    _expected_address: Address,
    _typed_data: &HardwareEip712Model,
    _mode: HardwareTypedDataSigningMode,
) -> Result<HardwareTypedDataSignOutcome> {
    Err(eyre!(
        "hardware public signing is not enabled in this build"
    ))
}

#[cfg(feature = "hardware")]
async fn probe_hardware_public_typed_data_signing_mode(
    descriptor: &HardwarePublicAccountDescriptor,
    hardware_session: &HardwareProfileSession,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    expected_address: Address,
) -> Result<(HardwareTypedDataSigningMode, Option<Vec<u8>>)> {
    match descriptor.device_kind {
        crate::hardware::HardwareDeviceKind::Ledger => {
            let client = crate::hardware::ledger::LedgerHardwareDerivationClient::connect()
                .await
                .wrap_err("connect Ledger for typed-data capability probe")?;
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .await
                .wrap_err("verify Ledger hardware profile")?;
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .await
                .wrap_err("verify Ledger public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let mode = client
                .typed_data_signing_mode()
                .await
                .wrap_err("probe Ledger typed-data capability")?;
            Ok((mode, None))
        }
        crate::hardware::HardwareDeviceKind::Trezor => {
            let mut client =
                crate::hardware::trezor::TrezorHardwareDerivationClient::connect_with_session(
                    hardware_session.trezor_session_id.clone(),
                )
                .wrap_err("connect Trezor for typed-data capability probe")?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .wrap_err("verify Trezor hardware profile")?;
            let trezor_session_id = active.trezor_session_id.clone();
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .wrap_err("verify Trezor public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let mode = client
                .typed_data_signing_mode()
                .wrap_err("probe Trezor typed-data capability")?;
            Ok((mode, trezor_session_id))
        }
    }
}

#[cfg(not(feature = "hardware"))]
#[allow(clippy::unused_async)]
async fn probe_hardware_public_typed_data_signing_mode(
    _descriptor: &HardwarePublicAccountDescriptor,
    _hardware_session: &HardwareProfileSession,
    _trezor_app_passphrase: Option<Zeroizing<String>>,
    _trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    _expected_address: Address,
) -> Result<(HardwareTypedDataSigningMode, Option<Vec<u8>>)> {
    Ok((HardwareTypedDataSigningMode::Unsupported, None))
}

#[cfg(feature = "hardware")]
async fn sign_hardware_public_transaction(
    descriptor: &HardwarePublicAccountDescriptor,
    hardware_session: &HardwareProfileSession,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    expected_address: Address,
    tx: &dyn SignableTransaction<Signature>,
) -> Result<(Signature, Option<Vec<u8>>)> {
    match descriptor.device_kind {
        crate::hardware::HardwareDeviceKind::Ledger => {
            let client = crate::hardware::ledger::LedgerHardwareDerivationClient::connect()
                .await
                .wrap_err("connect Ledger for public transaction signing")?;
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .await
                .wrap_err("verify Ledger hardware profile")?;
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .await
                .wrap_err("verify Ledger public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = client
                .sign_transaction_rlp(descriptor, &tx.encoded_for_signing())
                .await
                .wrap_err("sign public transaction on Ledger")?;
            Ok((signature, None))
        }
        crate::hardware::HardwareDeviceKind::Trezor => {
            let mut client =
                crate::hardware::trezor::TrezorHardwareDerivationClient::connect_with_session(
                    hardware_session.trezor_session_id.clone(),
                )
                .wrap_err("connect Trezor for public transaction signing")?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .wrap_err("verify Trezor hardware profile")?;
            let trezor_session_id = active.trezor_session_id.clone();
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .wrap_err("verify Trezor public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = client
                .sign_transaction(descriptor, tx)
                .wrap_err("sign public transaction on Trezor")?;
            Ok((signature, trezor_session_id))
        }
    }
}

#[cfg(not(feature = "hardware"))]
#[allow(clippy::unused_async)]
async fn sign_hardware_public_transaction(
    _descriptor: &HardwarePublicAccountDescriptor,
    _hardware_session: &HardwareProfileSession,
    _trezor_app_passphrase: Option<Zeroizing<String>>,
    _trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    _expected_address: Address,
    _tx: &dyn SignableTransaction<Signature>,
) -> Result<(Signature, Option<Vec<u8>>)> {
    Err(eyre!(
        "hardware public signing is not enabled in this build"
    ))
}

#[cfg(feature = "hardware")]
async fn sign_hardware_public_message(
    descriptor: &HardwarePublicAccountDescriptor,
    hardware_session: &HardwareProfileSession,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    expected_address: Address,
    message: &[u8],
) -> Result<(Signature, Option<Vec<u8>>)> {
    match descriptor.device_kind {
        crate::hardware::HardwareDeviceKind::Ledger => {
            let client = crate::hardware::ledger::LedgerHardwareDerivationClient::connect()
                .await
                .wrap_err("connect Ledger for public message signing")?;
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .await
                .wrap_err("verify Ledger hardware profile")?;
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .await
                .wrap_err("verify Ledger public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = client
                .sign_message(descriptor, message)
                .await
                .wrap_err("sign public message on Ledger")?;
            Ok((signature, None))
        }
        crate::hardware::HardwareDeviceKind::Trezor => {
            let mut client =
                crate::hardware::trezor::TrezorHardwareDerivationClient::connect_with_session(
                    hardware_session.trezor_session_id.clone(),
                )
                .wrap_err("connect Trezor for public message signing")?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let profile_path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)
                .wrap_err("parse hardware profile path")?;
            let active = client
                .active_profile_session(&profile_path)
                .wrap_err("verify Trezor hardware profile")?;
            let trezor_session_id = active.trezor_session_id.clone();
            ensure_hardware_public_profile_session(hardware_session, &active)?;
            let address = client
                .public_ethereum_address(descriptor)
                .wrap_err("verify Trezor public account address")?;
            ensure_hardware_public_address(expected_address, address)?;
            let signature = client
                .sign_message(descriptor, message)
                .wrap_err("sign public message on Trezor")?;
            Ok((signature, trezor_session_id))
        }
    }
}

#[cfg(not(feature = "hardware"))]
#[allow(clippy::unused_async)]
async fn sign_hardware_public_message(
    _descriptor: &HardwarePublicAccountDescriptor,
    _hardware_session: &HardwareProfileSession,
    _trezor_app_passphrase: Option<Zeroizing<String>>,
    _trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    _expected_address: Address,
    _message: &[u8],
) -> Result<(Signature, Option<Vec<u8>>)> {
    Err(eyre!(
        "hardware public signing is not enabled in this build"
    ))
}

#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
fn ensure_hardware_public_profile_session(
    expected: &HardwareProfileSession,
    actual: &HardwareProfileSession,
) -> Result<()> {
    if expected.device_kind == actual.device_kind && expected.binding == actual.binding {
        Ok(())
    } else {
        Err(eyre!(
            "hardware public signer profile mismatch: wrong device or passphrase context is active"
        ))
    }
}

#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
fn ensure_hardware_public_address(expected: Address, actual: Address) -> Result<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(eyre!(
            "hardware public account identity mismatch: expected {}, got {}",
            expected,
            actual
        ))
    }
}

pub(crate) fn vaulted_public_signer(
    vault_store: &DesktopVaultStore,
    view_session: &DesktopViewSession,
    vault_password: Option<&str>,
    public_account_uuid: &str,
    protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
) -> Result<VaultedPublicSigner> {
    let accounts = vault_store
        .list_public_accounts_for_session(view_session, true)
        .wrap_err("load public account metadata")?;
    let account = accounts
        .iter()
        .find(|account| account.public_account_uuid == public_account_uuid)
        .ok_or_else(|| eyre!("public account not found"))?;
    if account.source == crate::vault::PublicAccountSource::HardwareDerived {
        let descriptor = account
            .hardware_descriptor
            .clone()
            .ok_or_else(|| eyre!("hardware public account descriptor missing"))?;
        descriptor
            .validate()
            .map_err(|error| eyre!(error))
            .wrap_err("validate hardware public account descriptor")?;
        return Ok(VaultedPublicSigner::Hardware(HardwarePublicEvmSigner {
            address: account.address,
            descriptor,
            hardware_session: Mutex::new(
                view_session
                    .hardware_profile_session()
                    .cloned()
                    .ok_or_else(|| eyre!("hardware profile session required for public signer"))?,
            ),
            trezor_app_passphrase: Mutex::new(trezor_app_passphrase),
            trezor_pin_matrix_provider,
        }));
    }

    let vault_password = vault_password
        .ok_or_else(|| eyre!("vault password required for software public account signer"))?;
    let mut grant = vault_store
        .create_spend_grant(vault_password)
        .wrap_err("authorize public account spend")?;
    let private_key = vault_store
        .public_account_signing_key_with_session(
            &mut grant,
            view_session,
            public_account_uuid,
            protected_seed_session,
        )
        .wrap_err("load public account signing key")?;
    let signer = SoftwareEvmSigner::from_private_key(*private_key)
        .wrap_err("create public account signer")?;
    Ok(VaultedPublicSigner::Software(signer))
}
