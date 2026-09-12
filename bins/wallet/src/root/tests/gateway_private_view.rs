use super::*;
use crate::root::gateway_private_view::private_wallet_choices;

#[test]
fn private_wallet_choices_preserve_visibility_order_and_group_hardware() {
    use wallet_ops::vault::{
        HardwareRailgunAccountIdentity, HardwareRailgunAccountMetadata, WalletSoftwareContext,
    };
    let mut concealed = wallet_metadata(
        "hidden",
        "Concealed",
        WalletSource::Imported,
        WalletStatus::Active,
        0,
    );
    concealed.software_context = Some(WalletSoftwareContext::passphrase("base"));
    let mut hardware = wallet_metadata(
        "device",
        "Ledger account",
        WalletSource::LedgerDerived,
        WalletStatus::Active,
        2,
    );
    let descriptor = wallet_ops::hardware::HardwareDerivationDescriptor::ledger_eip1024_v1(
        wallet_ops::hardware::parse_bip32_path("m/44'/60'/0'/0/0").unwrap(),
        0,
        "ledger:synthetic".into(),
        wallet_ops::hardware::HardwareWalletSyncIntent::CreateNew,
    );
    hardware.hardware_account = Some(
        HardwareRailgunAccountMetadata::synthetic_software_v1(
            "profile",
            0,
            "Ledger account",
            descriptor,
            HardwareRailgunAccountIdentity {
                spending_public_key: [[0; 32]; 2],
                viewing_public_key: [0; 32],
            },
        )
        .with_receive_address("synthetic-hardware-receive"),
    );
    let mut second_ledger = hardware.clone();
    second_ledger.wallet_uuid = "second-ledger-profile".into();
    second_ledger.label = "Other Ledger profile".into();
    let mut trezor = hardware.clone();
    trezor.wallet_uuid = "trezor-profile".into();
    trezor.source = WalletSource::TrezorDerived;
    trezor.label = "Trezor profile".into();
    let mut second_trezor = trezor.clone();
    second_trezor.wallet_uuid = "second-trezor-profile".into();
    let metadata = vec![
        hardware,
        second_ledger,
        trezor,
        second_trezor,
        wallet_metadata(
            "inactive",
            "Inactive",
            WalletSource::Imported,
            WalletStatus::Inactive,
            0,
        ),
        concealed,
        wallet_metadata(
            "base",
            "Base",
            WalletSource::Imported,
            WalletStatus::Active,
            1,
        ),
        wallet_metadata(
            "legacy-hardware",
            "Unavailable hardware",
            WalletSource::LedgerDerived,
            WalletStatus::Active,
            3,
        ),
    ];
    let choices = private_wallet_choices(&metadata, None);
    assert_eq!(
        choices
            .iter()
            .map(|wallet| wallet.wallet_id.as_str())
            .collect::<Vec<_>>(),
        ["base", "hardware-device:ledger", "hardware-device:trezor"]
    );
    assert_eq!(choices[1].hardware.as_deref(), Some("ledger"));
    assert_eq!(choices[2].hardware.as_deref(), Some("trezor"));
    let serialized = serde_json::to_string(&choices).unwrap();
    assert!(!serialized.contains("Concealed"));
    assert!(!serialized.contains("account_identity"));
    assert!(!serialized.contains("profile"));
    assert!(!serialized.contains("receive_address"));
    assert!(!serialized.contains("synthetic-hardware-receive"));
    let choices = private_wallet_choices(&metadata, Some("hidden"));
    assert_eq!(choices[0].wallet_id, "hidden");
}
