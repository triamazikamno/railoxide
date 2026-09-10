mod client;
mod derivation;
mod error;
#[cfg(feature = "hardware")]
pub mod ledger;
mod path;
mod public_account;
#[cfg(feature = "hardware")]
pub mod trezor;
mod types;

pub use client::{HardwareDerivationClient, MockHardwareDerivationClient};
pub use derivation::{
    HardwareDerivationDescriptor, HardwareOperationOutput, HardwareViewAccessKey,
    SyntheticRailgunEntropy, hardware_profile_fingerprint,
    hardware_view_access_key_from_hardware_output, synthetic_entropy_from_hardware_output,
};
pub use error::HardwareDerivationError;
pub use path::{
    DEFAULT_HARDWARE_DERIVATION_PATH, HARDENED_BIP32_INDEX, format_bip32_path, parse_bip32_path,
};
pub use public_account::{
    ConfirmedHardwarePublicAccount, HardwarePublicAccountDescriptor, HardwarePublicAccountPathKind,
};
pub use types::{
    HardwareAppVersion, HardwareDerivationMethod, HardwareDeviceKind, HardwareTypedDataSigningMode,
    HardwareWalletSyncIntent,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn test_descriptor() -> HardwareDerivationDescriptor {
        HardwareDerivationDescriptor::ledger_eip1024_v1(
            parse_bip32_path("m/44'/60'/0'/0/0").expect("valid path"),
            0,
            "0x0123456789abcdef".to_owned(),
            HardwareWalletSyncIntent::CreateNew,
        )
    }

    #[test]
    fn path_roundtrip() {
        let path = parse_bip32_path("m/44'/60'/0'/0/0").expect("valid path");
        assert_eq!(format_bip32_path(&path), "m/44'/60'/0'/0/0");
    }

    #[test]
    fn hardware_public_account_paths_partition_by_wallet_account() {
        let trezor_zero = HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Trezor,
            0,
            0,
        )
        .expect("trezor wallet 0 public 0 path");
        let trezor_one = HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Trezor,
            1,
            2,
        )
        .expect("trezor wallet 1 public 2 path");
        let ledger_zero = HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Ledger,
            0,
            0,
        )
        .expect("ledger wallet 0 public 0 path");
        let ledger_one = HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Ledger,
            1,
            2,
        )
        .expect("ledger wallet 1 public 2 path");

        assert_eq!(trezor_zero.path_display(), "m/44'/60'/0'/0/0");
        assert_eq!(trezor_one.path_display(), "m/44'/60'/1'/0/2");
        assert_eq!(ledger_zero.path_display(), "m/44'/60'/0'/0/0");
        assert_eq!(ledger_one.path_display(), "m/44'/60'/1'/0/2");
    }

    #[test]
    fn hardware_derivation_descriptor_rejects_hardened_account_index() {
        let mut descriptor = test_descriptor();
        descriptor.account_index = HARDENED_BIP32_INDEX;

        assert!(matches!(
            descriptor.validate(),
            Err(HardwareDerivationError::InvalidDescriptor(
                "hardware wallet account index is too large"
            ))
        ));
    }

    #[test]
    fn legacy_hardware_public_account_descriptor_still_validates() {
        let descriptor: HardwarePublicAccountDescriptor = serde_json::from_str(
            r#"{
                "device_kind":"ledger",
                "path_kind":"ledger_live",
                "path":[2147483692,2147483708,2147483649,0,0],
                "account_index":1
            }"#,
        )
        .expect("legacy descriptor");

        assert_eq!(descriptor.wallet_account_index, 0);
        assert_eq!(descriptor.public_account_index, 1);
        descriptor.validate().expect("legacy descriptor validates");
    }

    #[cfg(feature = "hardware")]
    #[test]
    fn early_device_readiness_error_includes_trezor_no_device() {
        assert!(
            HardwareDerivationError::Trezor(trezor_client::Error::NoDeviceFound)
                .is_early_device_readiness_error()
        );
        assert!(HardwareDerivationError::TrezorLocked.is_early_device_readiness_error());
        assert!(
            HardwareDerivationError::UnsupportedTrezorPinMatrix.is_early_device_readiness_error()
        );
        assert!(
            HardwareDerivationError::Trezor(trezor_client::Error::TransportConnect(
                trezor_client::transport::error::Error::DeviceNotFound,
            ))
            .is_early_device_readiness_error()
        );
        assert!(
            !HardwareDerivationError::Trezor(trezor_client::Error::UnexpectedInteractionRequest(
                trezor_client::client::InteractionType::Button,
            ))
            .is_early_device_readiness_error()
        );
    }

    #[cfg(feature = "hardware")]
    #[test]
    fn user_rejection_requires_typed_cancellation() {
        use trezor_client::protos::failure::FailureType;

        for (code, expected) in [
            (FailureType::Failure_ActionCancelled, true),
            (FailureType::Failure_PinCancelled, true),
            (FailureType::Failure_DataError, false),
            (FailureType::Failure_PinInvalid, false),
        ] {
            let mut failure = trezor_client::protos::Failure::new();
            failure.set_code(code);
            failure.set_message("cancelled/rejected".to_owned());
            let error =
                HardwareDerivationError::Trezor(trezor_client::Error::FailureResponse(failure));
            assert_eq!(error.is_user_rejected(), expected);
        }

        for (status, expected) in [
            (0x6982, true),
            (0x6985, true),
            (0x6a80, false),
            (0x6511, false),
        ] {
            let error = HardwareDerivationError::LedgerStatus {
                operation: "sign",
                status,
                message: "cancelled/rejected",
            };
            assert_eq!(error.is_user_rejected(), expected);
        }

        assert!(HardwareDerivationError::TrezorPinEntryCancelled.is_user_rejected());
        assert!(!HardwareDerivationError::TrezorLocked.is_user_rejected());
        assert!(
            !HardwareDerivationError::TrezorBridge("cancelled/rejected".to_owned())
                .is_user_rejected()
        );
    }

    #[test]
    fn descriptor_debug_redacts_fingerprint() {
        let descriptor = test_descriptor();
        let debug = format!("{descriptor:?}");
        assert!(!debug.contains("0123456789abcdef"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn synthetic_entropy_is_deterministic_for_pure_vector() {
        let descriptor = test_descriptor();
        let mut hardware_output = [0u8; 32];
        for (index, byte) in hardware_output.iter_mut().enumerate() {
            *byte = u8::try_from(index).expect("index fits in u8");
        }
        let first = synthetic_entropy_from_hardware_output(
            &descriptor,
            HardwareOperationOutput::new(hardware_output),
        )
        .expect("derive entropy");
        let second = synthetic_entropy_from_hardware_output(
            &descriptor,
            HardwareOperationOutput::new(hardware_output),
        )
        .expect("derive entropy");
        assert_eq!(first.expose_secret(), second.expose_secret());
        assert_eq!(
            first.expose_secret(),
            &[
                0xf6, 0x87, 0x45, 0x84, 0x46, 0xa8, 0x16, 0x9e, 0xfb, 0x58, 0x6c, 0x3c, 0x75, 0xe6,
                0x9b, 0x0e, 0xeb, 0xde, 0xec, 0xb9, 0x6d, 0xf9, 0x9d, 0x17, 0xfc, 0xcf, 0xe3, 0xe9,
                0xf5, 0x80, 0x9f, 0x26,
            ],
        );
    }

    #[tokio::test]
    async fn mock_client_derives_synthetic_entropy() {
        let descriptor = test_descriptor();
        let mut mock = MockHardwareDerivationClient::new([[7u8; 32]]);
        let entropy = mock
            .derive_synthetic_entropy(&descriptor)
            .await
            .expect("derive entropy");
        assert_ne!(entropy.expose_secret(), &[0u8; 32]);
    }

    #[cfg(feature = "hardware")]
    #[test]
    fn trezor_bridge_message_framing_roundtrip() {
        let encoded = trezor::encode_bridge_message(trezor_client::transport::ProtoMessage::new(
            trezor_client::protos::MessageType::MessageType_Initialize,
            vec![1, 2, 3],
        ));
        assert_eq!(encoded, "000000000003010203");

        let bytes = alloy::hex::decode(encoded).expect("hex bridge frame");
        let decoded = trezor::decode_bridge_message(&bytes).expect("decode bridge frame");
        assert_eq!(
            decoded.message_type(),
            trezor_client::protos::MessageType::MessageType_Initialize,
        );
        assert_eq!(decoded.payload(), &[1, 2, 3]);
    }

    #[cfg(feature = "hardware")]
    #[test]
    fn trezor_bridge_selection_rejects_busy_or_ambiguous_devices() {
        let free = trezor::BridgeDevice {
            path: "device-1".to_owned(),
            session: None,
        };
        let busy = trezor::BridgeDevice {
            path: "device-1".to_owned(),
            session: Some("session".to_owned()),
        };

        assert!(matches!(
            trezor::select_bridge_device(&[]),
            Err(trezor::BridgeConnectError::NoDevice)
        ));
        assert!(matches!(
            trezor::select_bridge_device(std::slice::from_ref(&busy)),
            Err(trezor::BridgeConnectError::DeviceBusy)
        ));
        assert!(matches!(
            trezor::select_bridge_device(&[free.clone(), busy]),
            Err(trezor::BridgeConnectError::DeviceNotUnique(2))
        ));
        let selected = trezor::select_bridge_device(&[free]).expect("select one free device");
        assert_eq!(selected.path, "device-1");
    }

    #[cfg(feature = "hardware")]
    #[test]
    fn trezor_bridge_busy_message_points_to_competing_apps() {
        let message = trezor::trezor_bridge_busy_message();

        assert!(message.contains("Trezor Suite"));
        assert!(message.contains("browser wallet tabs"));
        assert!(message.contains("other Trezor applications"));
    }
}
