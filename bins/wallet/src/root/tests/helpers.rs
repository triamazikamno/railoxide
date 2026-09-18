pub(super) use std::collections::{BTreeMap, BTreeSet};
pub(super) use std::fs;
pub(super) use std::path::PathBuf;
pub(super) use std::sync::Arc;
pub(super) use std::time::{Duration, SystemTime};

pub(super) use alloy::primitives::{Address, U256, address};
pub(super) use alloy::uint;
pub(super) use broadcaster_monitor::FeeRow;
pub(super) use broadcaster_monitor_waku::{DEFAULT_DOH_ENDPOINT, DEFAULT_TOR_DOH_ENDPOINT};
pub(super) use gpui_component::select::SelectItem;
pub(super) use wallet_ops::{
    BlockedShieldRescueInfo, DesktopNativeTopUpPlan, FeeHandlingMode, ListUtxosOutput,
    PublicAccountBalance, PublicActionProgressStep, PublicAssetId, PublicBalanceAmount,
    PublicBalanceAsset, PublicBalanceEntry, PublicBroadcasterCandidate,
    PublicBroadcasterCostEstimate, PublicBroadcasterFeeMargin, PublicBroadcasterResultKind,
    PublicBroadcasterSelection, PublicBroadcasterSubmissionResult, SyncProgressStage,
    SyncProgressUpdate, TransactionGenerationStage, UtxoOutput, UtxoPpoiState,
    settings::{
        BuiltInTokenOverride, CustomTokenSettings, NetworkModeSetting, PoiReadSourceSetting,
        PriceAnchorSettings, TokenKey, TokenPriceAnchorOverride, WALLET_SETTINGS_KEY,
        WakuDirectPeerSetting, WalletSettings, build_effective_chain_configs,
        build_effective_token_registry, default_chain_contract_settings,
        default_chain_quick_sync_endpoint, default_chain_rpc_endpoints, default_waku_direct_peers,
        default_waku_dns_enr_trees, encode_wallet_settings,
    },
    vault::{
        PublicAccountScope, PublicAccountSource, PublicAccountStatus, WalletMetadataBundle,
        WalletSource, WalletStatus,
    },
};

use super::super::private_broadcaster::PrivateSubmissionProgressFlow;
use super::super::{
    DeliveryFormKind, PrivateBroadcasterProgressState, UnshieldAssetKey,
    private_broadcaster_progress_steps, self_broadcast_progress_steps,
};

pub(super) fn utxo_output(token: &str, value: &str, is_spent: bool) -> UtxoOutput {
    const SOURCE_TX_HASH: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";
    const SPENT_TX_HASH: &str =
        "0x2222222222222222222222222222222222222222222222222222222222222222";

    utxo_output_with_hashes(
        token,
        value,
        is_spent,
        SOURCE_TX_HASH,
        is_spent.then_some(SPENT_TX_HASH),
    )
}

pub(super) fn utxo_output_with_hashes(
    token: &str,
    value: &str,
    is_spent: bool,
    source_tx_hash: &str,
    spent_tx_hash: Option<&str>,
) -> UtxoOutput {
    UtxoOutput {
        tree: 0,
        position: 7,
        token: token.to_string(),
        value: value.to_string(),
        commitment_kind: "Transact".to_string(),
        activity_classification: "Private Output".to_string(),
        blocked_shield_rescue: None,
        commitment: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        npk: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        blinded_commitment: "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            .to_string(),
        poi_statuses: BTreeMap::from([(
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
            if is_spent { "Unknown" } else { "Valid" }.to_string(),
        )]),
        ppoi_state: if is_spent {
            UtxoPpoiState::Unknown
        } else {
            UtxoPpoiState::Valid
        },
        ppoi_last_submission_at: None,
        poi_spendable: !is_spent,
        source_tx_hash: source_tx_hash.to_string(),
        source_block_number: 11,
        source_block_timestamp: 1_700_000_011,
        is_spent,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        spent_tx_hash: spent_tx_hash.map(str::to_string),
        spent_block_number: spent_tx_hash.map(|_| 21),
    }
}

pub(in crate::root) fn unshield_utxo_output(
    token: Address,
    value: u64,
    tree: u32,
    position: u64,
) -> UtxoOutput {
    UtxoOutput {
        tree,
        position,
        token: token.to_checksum(None),
        value: value.to_string(),
        commitment_kind: "Transact".to_string(),
        activity_classification: "Private Output".to_string(),
        blocked_shield_rescue: None,
        commitment: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        npk: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        blinded_commitment: "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            .to_string(),
        poi_statuses: BTreeMap::from([(
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
            "Valid".to_string(),
        )]),
        ppoi_state: UtxoPpoiState::Valid,
        ppoi_last_submission_at: None,
        poi_spendable: true,
        source_tx_hash: "0x1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        source_block_number: 11,
        source_block_timestamp: 1_700_000_011,
        is_spent: false,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        spent_tx_hash: None,
        spent_block_number: None,
    }
}

pub(super) fn temp_wallet_db_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "railoxide-wallet-root-tests-{name}-{}-{nanos}",
        std::process::id()
    ))
}

pub(in crate::root) fn fee_row(chain_id: u64, token: Address, fees_id: &str) -> FeeRow {
    const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

    FeeRow {
        chain_id,
        railgun_address: Arc::from(RAILGUN_ADDRESS),
        token_address: token,
        fee: uint!(10_U256),
        signature_valid: true,
        fees_id: Arc::from(fees_id),
        fee_expiration: SystemTime::now() + Duration::from_mins(1),
        available_wallets: 1,
        version: Arc::from("8.2.3"),
        relay_adapt: Address::ZERO,
        relay_adapt_7702: None,
        required_poi_list_keys: Vec::new(),
        identifier: Some(Arc::from(fees_id)),
        last_seen: SystemTime::now(),
        reliability: 0.9,
    }
}

pub(super) fn public_broadcaster_cost_estimate(
    candidate: PublicBroadcasterCandidate,
) -> PublicBroadcasterCostEstimate {
    PublicBroadcasterCostEstimate {
        broadcaster: candidate,
        action_token: Address::from([0x41; 20]),
        fee_token: Address::from([0x42; 20]),
        entered_amount: U256::from(1),
        receiver_amount: U256::from(1),
        recipient_amount: U256::from(1),
        total_private_spend: U256::from(1),
        fee_amount: U256::from(1),
        protocol_fee_amount: U256::ZERO,
        protocol_fee_bps: U256::ZERO,
        fee_mode: FeeHandlingMode::AddToAmount,
        max_receiver_amount: U256::from(1),
        max_entered_amount: U256::from(1),
        gas_limit: 1,
        min_gas_price: 1,
        native_gas_cost: U256::from(1),
        transaction_count: 1,
        input_count: 1,
        private_output_count: 2,
        public_output_count: 0,
        relay_call_count: 0,
        uses_relay_adapt: false,
        native_top_up: None,
    }
}

pub(super) fn private_progress_state(
    flow: PrivateSubmissionProgressFlow,
    key: UnshieldAssetKey,
) -> PrivateBroadcasterProgressState {
    PrivateBroadcasterProgressState {
        gateway_execution: None,
        stealth_account: None,
        flow,
        kind: DeliveryFormKind::Send,
        key,
        generation_id: 7,
        asset_label: Arc::from("ETH"),
        icon_path: None,
        recipient: Arc::from("0zk"),
        recipient_output: None,
        gas_payer: None,
        requires_device_approval: false,
        sponsored_funding: false,
        steps: match flow {
            PrivateSubmissionProgressFlow::PublicBroadcaster => {
                private_broadcaster_progress_steps()
            }
            PrivateSubmissionProgressFlow::SelfBroadcast => {
                self_broadcast_progress_steps(DeliveryFormKind::Send)
            }
        },
        estimate: None,
        result: None,
        self_broadcast_result: None,
        self_broadcast_command_tx: None,
        sponsored_self_broadcast_command_tx: None,
        sponsored_self_broadcast_outcome: None,
        self_broadcast_attempts: Vec::new(),
        self_broadcast_current_gas_fee: None,
        self_broadcast_action_error: None,
        public_broadcaster_response_timeout: Some(Duration::from_mins(2)),
        public_broadcaster_republish_interval: Some(Duration::from_secs(5)),
        public_broadcaster_wait_started_at: None,
        task_abort_handle: None,
        stop_available: true,
        stopped: false,
        error: None,
        dialog_open: false,
        stage_seen: false,
    }
}

pub(super) fn wallet_metadata(
    wallet_uuid: &str,
    label: &str,
    source: WalletSource,
    status: WalletStatus,
    display_order: u32,
) -> WalletMetadataBundle {
    WalletMetadataBundle {
        wallet_uuid: wallet_uuid.to_string(),
        label: label.to_string(),
        derivation_index: 0,
        source,
        status,
        display_order,
        hardware_descriptor: None,
        hardware_account: None,
        pending_create_new_chain_ids: BTreeSet::new(),
        software_context: (!source.is_hardware_derived())
            .then(|| wallet_ops::vault::WalletSoftwareContext::standard(wallet_uuid)),
    }
}
