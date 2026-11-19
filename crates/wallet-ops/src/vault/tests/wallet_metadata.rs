use super::super::*;
use super::helpers::*;
use crate::DesktopWalletVaultDbExt as _;
use alloy::primitives::FixedBytes;
use local_db::{
    OpaqueWalletPrivateRow, OutputPoiRecoveryRecord, OutputPoiRecoveryStatus,
    PendingOutputPoiContextRecord, PendingOutputPoiRole, WalletMeta, WalletPrivateNamespaceId,
    WalletPrivateRecordKind, WalletSyncActorStateRecord,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;

#[derive(Serialize)]
struct ReleasedWalletMetadataWithoutContext {
    wallet_uuid: String,
    label: String,
    derivation_index: u32,
    source: WalletSource,
    status: WalletStatus,
    display_order: u32,
    hardware_descriptor: Option<HardwareDerivationDescriptor>,
    hardware_account: Option<HardwareRailgunAccountMetadata>,
    pending_create_new_chain_ids: BTreeSet<u64>,
}

mod chains;
mod creation_and_listing;
mod management_and_deletion;
