use super::*;
use crate::root::chain_load::SyncStatusContext;
use crate::root::public_action::{
    public_action_progress_steps_for_source, public_unwrap_gas_error,
};
use crate::root::public_balances::{
    public_account_usd_total_label_for_chain, public_balance_entry_for_chain,
};
use crate::root::shell::{balance_sync_data_source, balance_sync_source_label};
use wallet_ops::{
    PublicBroadcasterFeeBreakdown, PublicScanSource, SyncProgressStage, SyncProgressUpdate,
};

fn public_account_for_search(label: Option<&str>, address: Address) -> PublicAccountMetadata {
    public_account_for_search_with_uuid("public-account", label, address)
}

fn public_account_for_search_with_uuid(
    uuid: &str,
    label: Option<&str>,
    address: Address,
) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: uuid.to_string(),
        address,
        label: label.map(str::to_string),
        source: PublicAccountSource::Imported,
        scope: PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    }
}

fn public_balance_snapshot_for_test(chain_id: u64) -> PublicBalanceSnapshot {
    let account = public_account_for_search(Some("Main account"), Address::from([0x11; 20]));
    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
            observed_at: None,
            observed_block: None,
            account,
            balances: vec![PublicBalanceEntry {
                asset: PublicBalanceAsset {
                    id: PublicAssetId::Native,
                    symbol: "ETH".to_string(),
                    decimals: 18,
                },
                amount: PublicBalanceAmount::Available(U256::from(5_u64)),
            }],
        }],
    }
}

fn public_native_balance_snapshot_for_test(
    chain_id: u64,
    accounts: Vec<(PublicAccountMetadata, PublicBalanceAmount)>,
) -> PublicBalanceSnapshot {
    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: accounts
            .into_iter()
            .map(|(account, amount)| PublicAccountBalance {
                observed_at: None,
                observed_block: None,
                account,
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".to_string(),
                        decimals: 18,
                    },
                    amount,
                }],
            })
            .collect(),
    }
}

mod account_selection;
mod fees_qr_identicons;
mod labels_and_costs;
mod native_top_up;
mod private_action_assets;
mod public_action_progress;
mod public_balances;
mod public_lists;
