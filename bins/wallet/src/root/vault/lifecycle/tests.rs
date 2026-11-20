use super::{
    clear_wallet_context_visibility, pending_profile_result_is_current,
    pending_software_profile_open_timeout_is_current, wallet_replacement_finalize_is_admitted,
};
use crate::root::WalletOption;
use std::collections::BTreeSet;
use std::sync::Arc;
use wallet_ops::vault::{
    PublicAccountMetadata, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
    WalletMetadataBundle, WalletSoftwareContext,
};

#[test]
fn pending_profile_timeout_ignores_stale_lifetime_or_state() {
    assert!(pending_software_profile_open_timeout_is_current(7, 7, true));
    assert!(!pending_software_profile_open_timeout_is_current(
        7, 8, true
    ));
    assert!(!pending_software_profile_open_timeout_is_current(
        7, 7, false
    ));
}

#[test]
fn wallet_replacement_finalize_requires_current_switching_admission() {
    assert!(wallet_replacement_finalize_is_admitted(
        7, 7, 1, 1, false, true
    ));
    assert!(!wallet_replacement_finalize_is_admitted(
        6, 7, 1, 1, false, true
    ));
    assert!(!wallet_replacement_finalize_is_admitted(
        7, 7, 2, 1, false, true
    ));
    assert!(!wallet_replacement_finalize_is_admitted(
        7, 7, 1, 1, true, true
    ));
    assert!(!wallet_replacement_finalize_is_admitted(
        7, 7, 1, 1, false, false
    ));
}

#[test]
fn pending_profile_result_rejects_every_replacement_boundary() {
    let current = |operation, wallet, profile, selected, chain, pending| {
        pending_profile_result_is_current(
            operation,
            7,
            wallet,
            11,
            "base-profile",
            profile,
            selected,
            chain,
            1,
            pending,
        )
    };

    assert!(current(7, 11, Some("base-profile"), None, 1, true));
    assert!(!current(6, 11, Some("base-profile"), None, 1, true));
    assert!(!current(7, 10, Some("base-profile"), None, 1, true));
    assert!(!current(7, 11, Some("other-profile"), None, 1, true));
    assert!(!current(
        7,
        11,
        Some("base-profile"),
        Some("selected"),
        1,
        true
    ));
    assert!(!current(7, 11, Some("base-profile"), None, 2, true));
    assert!(!current(7, 11, Some("base-profile"), None, 1, false));
}

#[test]
fn pending_context_cleanup_clears_visible_state() {
    let mut view_session = Some(Arc::new(()));
    let mut selected_wallet_id = Some(Arc::from("child"));
    let mut revealed = Some(Arc::from("child"));
    let mut metadata = vec![WalletMetadataBundle {
        wallet_uuid: "child".to_owned(),
        label: "Child".to_owned(),
        derivation_index: 0,
        source: wallet_ops::vault::WalletSource::Imported,
        status: wallet_ops::vault::WalletStatus::Active,
        display_order: 0,
        hardware_descriptor: None,
        hardware_account: None,
        pending_create_new_chain_ids: BTreeSet::default(),
        software_context: Some(WalletSoftwareContext::passphrase("base")),
    }];
    let mut options = vec![WalletOption {
        wallet_id: Arc::from("child"),
        source: wallet_ops::vault::WalletSource::Imported,
    }];
    let mut accounts = vec![PublicAccountMetadata {
        public_account_uuid: "child-account".to_owned(),
        address: alloy::primitives::Address::ZERO,
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "child".to_owned(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    }];

    clear_wallet_context_visibility(
        &mut view_session,
        &mut selected_wallet_id,
        &mut revealed,
        &mut metadata,
        &mut options,
        &mut accounts,
    );

    assert!(view_session.is_none());
    assert!(selected_wallet_id.is_none());
    assert!(revealed.is_none());
    assert_eq!(metadata, [] as [WalletMetadataBundle; 0]);
    assert!(options.is_empty());
    assert_eq!(accounts, [] as [PublicAccountMetadata; 0]);
}
