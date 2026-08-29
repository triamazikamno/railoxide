use std::collections::{BTreeMap, BTreeSet};

use wallet_ops::vault::PublicAccountMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParticipantResolution {
    pub(super) uuids: Vec<String>,
    pub(super) changed: bool,
}

pub(super) fn normalize_participant_ids(
    persisted: &[String],
    visible_accounts: &[PublicAccountMetadata],
    wallet_uuid: &str,
) -> ParticipantResolution {
    let mut seen = BTreeSet::new();
    let uuids = persisted
        .iter()
        .filter(|uuid| seen.insert(uuid.as_str()))
        .filter(|uuid| {
            visible_accounts.iter().any(|account| {
                account.public_account_uuid == uuid.as_str()
                    && account.is_scoped_to_wallet(wallet_uuid)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    ParticipantResolution {
        changed: uuids != persisted,
        uuids,
    }
}

pub(super) fn remove_scoped_participant(
    participants: &mut BTreeMap<String, Vec<String>>,
    wallet_uuid: &str,
    public_account_uuid: &str,
) -> bool {
    remove_uuid_from_wallet(participants, wallet_uuid, public_account_uuid)
}

pub(super) fn remove_global_participant(
    participants: &mut BTreeMap<String, Vec<String>>,
    public_account_uuid: &str,
) -> bool {
    let mut changed = false;
    for uuids in participants.values_mut() {
        let original_len = uuids.len();
        uuids.retain(|uuid| uuid != public_account_uuid);
        changed |= uuids.len() != original_len;
    }
    changed
}

pub(super) fn remove_private_wallet_participants(
    participants: &mut BTreeMap<String, Vec<String>>,
    wallet_uuid: &str,
) -> bool {
    participants.remove(wallet_uuid).is_some()
}

fn remove_uuid_from_wallet(
    participants: &mut BTreeMap<String, Vec<String>>,
    wallet_uuid: &str,
    public_account_uuid: &str,
) -> bool {
    let Some(uuids) = participants.get_mut(wallet_uuid) else {
        return false;
    };
    let original_len = uuids.len();
    uuids.retain(|uuid| uuid != public_account_uuid);
    uuids.len() != original_len
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use wallet_ops::vault::{
        PublicAccountMetadata, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
    };

    use super::*;

    fn account(
        uuid: &str,
        _wallet_uuid: &str,
        scope: PublicAccountScope,
        status: PublicAccountStatus,
    ) -> PublicAccountMetadata {
        PublicAccountMetadata {
            public_account_uuid: uuid.to_owned(),
            address: address!("0x1111111111111111111111111111111111111111"),
            label: Some(uuid.to_owned()),
            source: PublicAccountSource::Imported,
            scope,
            derivation_index: None,
            hardware_descriptor: None,
            status,
            display_order: 0,
        }
    }

    #[test]
    fn normalization_preserves_order_and_inactive_visible_accounts() {
        let visible = vec![
            account(
                "active",
                "wallet-a",
                PublicAccountScope::PrivateWallet {
                    wallet_uuid: "wallet-a".to_owned(),
                },
                PublicAccountStatus::Active,
            ),
            account(
                "inactive",
                "wallet-a",
                PublicAccountScope::PrivateWallet {
                    wallet_uuid: "wallet-a".to_owned(),
                },
                PublicAccountStatus::Inactive,
            ),
            account(
                "global",
                "wallet-b",
                PublicAccountScope::Global,
                PublicAccountStatus::Active,
            ),
            account(
                "other-wallet",
                "wallet-b",
                PublicAccountScope::PrivateWallet {
                    wallet_uuid: "wallet-b".to_owned(),
                },
                PublicAccountStatus::Active,
            ),
        ];
        let resolution = normalize_participant_ids(
            &[
                "inactive".to_owned(),
                "missing".to_owned(),
                "active".to_owned(),
                "inactive".to_owned(),
                "other-wallet".to_owned(),
                "global".to_owned(),
            ],
            &visible,
            "wallet-a",
        );
        assert_eq!(
            resolution.uuids,
            vec![
                "inactive".to_owned(),
                "active".to_owned(),
                "global".to_owned()
            ]
        );
        assert!(resolution.changed);
    }

    #[test]
    fn unchanged_normalization_reports_no_persisted_change() {
        let visible = vec![account(
            "account",
            "wallet-a",
            PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet-a".to_owned(),
            },
            PublicAccountStatus::Active,
        )];
        let resolution = normalize_participant_ids(&["account".to_owned()], &visible, "wallet-a");
        assert_eq!(resolution.uuids, vec!["account".to_owned()]);
        assert!(!resolution.changed);
    }

    #[test]
    fn cleanup_respects_scope_and_wallet_lifecycle() {
        let mut participants = BTreeMap::from([
            (
                "wallet-a".to_owned(),
                vec!["scoped".to_owned(), "global".to_owned()],
            ),
            (
                "wallet-b".to_owned(),
                vec!["global".to_owned(), "other".to_owned()],
            ),
        ]);
        assert!(remove_scoped_participant(
            &mut participants,
            "wallet-a",
            "scoped"
        ));
        assert_eq!(participants["wallet-a"], vec!["global".to_owned()]);
        assert_eq!(
            participants["wallet-b"],
            vec!["global".to_owned(), "other".to_owned()]
        );
        assert!(remove_global_participant(&mut participants, "global"));
        assert_eq!(participants["wallet-a"], Vec::<String>::new());
        assert_eq!(participants["wallet-b"], vec!["other".to_owned()]);
        assert!(remove_private_wallet_participants(
            &mut participants,
            "wallet-a"
        ));
        assert!(!participants.contains_key("wallet-a"));
        assert!(!remove_private_wallet_participants(
            &mut participants,
            "wallet-a"
        ));
    }
}
