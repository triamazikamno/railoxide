pub(super) use crate::amounts::wrapped_native_token_for_chain;
pub(super) use std::collections::{BTreeMap, BTreeSet};
pub(super) use std::fs;
pub(super) use std::path::PathBuf;
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::{AtomicU64, Ordering};
pub(super) use std::time::{Duration, SystemTime};

pub(super) use alloy::hex;
pub(super) use alloy::primitives::{Address, Bytes, FixedBytes, TxHash, U256};
pub(super) use alloy::uint;
pub(super) use broadcaster_core::crypto::railgun::{
    Address as RailgunAddress, AddressData, ViewingKeyData,
};
pub(super) use broadcaster_core::deployment::RailgunDeployment;
pub(super) use broadcaster_core::notes::Note;
pub(super) use broadcaster_core::transact::{
    EncryptedTransactRequest, PreTxPoi, SnarkJsProof, railgun_txid_leaf_hash,
    try_decrypt_transact_request,
};
pub(super) use broadcaster_core::transact_response::DecryptedTransactResponse;
pub(super) use broadcaster_core::tree::TREE_DEPTH;
pub(super) use broadcaster_monitor::FeeRow;
pub(super) use local_db::{DbConfig, DbStore, PendingOutputPoiRole};
pub(super) use merkletree::tree::MerkleProof;
pub(super) use poi::poi::default_active_poi_list_keys;
pub(super) use railgun_wallet::tx::{
    BuildError, InputWitness, PrivateInputs, PublicInputs, TransactionCall, TransactionPlanChunk,
    UnshieldPlan, UnshieldSelectionInfo,
};
pub(super) use railgun_wallet::{
    PoiStatus, Utxo, UtxoCommitmentKind, UtxoSource, WalletKeys, WalletUtxo,
};
pub(super) use serde_json::json;

pub(super) use crate::desktop::random_eligible_public_broadcasters;
pub(super) use crate::hardware::{
    HardwareDerivationDescriptor, HardwareWalletSyncIntent, parse_bip32_path,
};
pub(super) use crate::signer::{EvmMessageSigner, EvmTransactionSigner, SoftwareEvmSigner};
pub(super) use crate::{
    ApproximateTransactionShape, BlockedShieldRescueUtxoId, BroadcasterFeePolicy,
    BroadcasterFeePolicyStatus, CompositeRelayAction, CompositeRelayActionToken,
    CompositeUnshieldLegRole, CompositeUnshieldRecipient, CreatedWalletChainInitPolicy,
    DesktopNativeTopUpPlan, DesktopWalletChainStart, DesktopWalletSyncStartPolicy, FeeHandlingMode,
    ListUtxosOutput, PublicBroadcasterCandidate, PublicBroadcasterFeeMargin,
    PublicBroadcasterResultKind, PublicBroadcasterSelection, PublicBroadcasterTrustFilter,
    RAILGUN_PROTOCOL_FEE_BPS, RpcChainRoute, SelfBroadcastFeeSample, SelfBroadcastGasFeeQuote,
    SelfBroadcastGasFeeSelection, SelfBroadcastTipFallback, TokenTotal, UtxoOutput, UtxoPpoiState,
    WalletPendingOverlay, WalletPendingSpent, WalletPendingSpentMarkOutcome,
    apply_pending_overlay_to_outputs, approximate_public_broadcaster_cost,
    approximate_public_broadcaster_gas, broadcaster_fee_amount, broadcaster_fee_covers,
    buffered_public_broadcaster_fee, decode_public_broadcaster_response,
    eligible_public_broadcasters, fee_policy_eligible_public_broadcasters,
    filter_public_broadcasters_by_trust, fixed_token_anchor_rate,
    initial_separate_token_public_broadcaster_fee,
    initialize_new_wallet_chain_metadata_for_session,
    is_self_broadcast_insufficient_native_gas_error, is_self_broadcast_tx_already_known_message,
    is_wrapped_native_token, max_broadcaster_fee_token_amount_from_outputs,
    max_send_amount_from_outputs, max_unshield_amount_from_outputs,
    native_top_up_approximate_shape, native_top_up_composite_unshield_request,
    native_top_up_policy_for_chain, native_top_up_primary_recipient_amount_for_fee_mode,
    native_top_up_required_wrapped_native_amount, native_top_up_wrapped_native_amount,
    new_wallet_chain_start_from_deployment, new_wallet_chain_start_from_head,
    parse_railgun_recipient, parse_send_amount, parse_submitted_tx_hash, parse_unshield_amount,
    public_broadcaster_amount_split, public_broadcaster_amount_split_for_tokens,
    public_broadcaster_amount_split_for_tokens_and_protocol,
    public_broadcaster_anchor_rate_for_policy, public_broadcaster_bound_min_gas_price,
    public_broadcaster_build_error, public_broadcaster_candidates,
    public_broadcaster_fee_breakdown, public_broadcaster_gas_limit_with_buffer,
    public_broadcaster_max_entered_amount, public_broadcaster_max_entered_amount_for_tokens,
    public_broadcaster_max_entered_amount_for_tokens_and_protocol,
    public_broadcaster_reported_amounts, public_broadcaster_republish_loop,
    public_broadcaster_transact_params, resolve_desktop_wallet_chain_start,
    resolve_self_broadcast_gas_fee, select_public_broadcaster,
    select_public_broadcaster_with_policy, select_public_broadcaster_with_policy_and_trust,
    self_broadcast_gas_limit_with_buffer, self_broadcast_insufficient_native_gas_error,
    self_broadcast_native_gas_cost, self_broadcast_preflight_error_message,
    self_broadcast_quote_from_fee_samples, self_broadcast_quote_from_fee_samples_with_tip_fallback,
    self_broadcast_transaction_request, send_approximate_shape, sort_specific_public_broadcasters,
    transact_topic, unshield_approximate_shape, utxo_outputs_from_utxos,
    validate_self_broadcast_gas_fee, vault,
};

pub(super) static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);
pub(super) const TEST_PASSWORD: &str = "correct horse battery staple";
pub(super) const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

pub(super) fn address(byte: u8) -> Address {
    Address::from_slice(&[byte; 20])
}

pub(super) fn temp_db_root() -> PathBuf {
    let dir = std::env::temp_dir().join("railoxide-wallet-ops-tests");
    fs::create_dir_all(&dir).expect("create temp db dir");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("db-{pid}-{nanos}-{counter}"))
}

pub(super) fn source(byte: u8) -> UtxoSource {
    UtxoSource {
        tx_hash: FixedBytes::from([byte; 32]),
        block_number: u64::from(byte),
        block_timestamp: 1_700_000_000 + u64::from(byte),
    }
}

pub(super) fn hardware_wallet_metadata(
    sync_intent: HardwareWalletSyncIntent,
) -> vault::WalletMetadataBundle {
    let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
        parse_bip32_path("m/44'/60'/0'/0/0").expect("valid path"),
        0,
        "ledger:evm:0x1111111111111111111111111111111111111111".to_string(),
        sync_intent,
    );
    vault::WalletMetadataBundle {
        wallet_uuid: "hardware-wallet".to_string(),
        label: "Hardware wallet".to_string(),
        derivation_index: descriptor.account_index,
        source: vault::WalletSource::LedgerDerived,
        status: vault::WalletStatus::Active,
        display_order: 0,
        hardware_descriptor: Some(descriptor),
        hardware_account: None,
        pending_create_new_chain_ids: BTreeSet::new(),
        software_context: None,
    }
}

pub(super) fn effective_chain_config_with_rpc_endpoints(
    chain_id: u64,
    rpc_endpoints: Vec<String>,
    deployment_block: u64,
) -> crate::settings::EffectiveChainConfig {
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(chain_id)
            .cloned()
            .unwrap();
    config.rpc_route = RpcChainRoute::new(
        chain_id,
        rpc_endpoints
            .into_iter()
            .map(|url| reqwest::Url::parse(&url).unwrap())
            .collect(),
    )
    .with_multicall(config.rpc_route.multicall().unwrap());
    let private = config.railgun.as_mut().unwrap();
    private.deployment.deployment_block = deployment_block;
    private.indexed_artifact_source_mode =
        crate::settings::IndexedArtifactSourceModeSetting::Disabled;
    private.sync.indexed_artifact_source = None;
    config
}

pub(super) fn selection_info(
    total: U256,
    input_count: usize,
    transaction_count: usize,
    private_output_count: usize,
    public_output_count: usize,
    max_spendable: U256,
) -> UnshieldSelectionInfo {
    UnshieldSelectionInfo {
        total,
        input_count,
        transaction_count,
        private_output_count,
        public_output_count,
        max_spendable,
    }
}

pub(super) fn sample_railgun_address(seed: u8) -> String {
    let viewing = ViewingKeyData::from_spending_public_key(
        [seed; 32],
        [U256::from(seed), U256::from(seed + 1)],
    );
    viewing
        .derive_address(None)
        .expect("derive railgun address")
        .to_string()
}

pub(super) fn sample_public_broadcaster_candidate(
    seed: u8,
) -> (PublicBroadcasterCandidate, ViewingKeyData) {
    let viewing = ViewingKeyData::from_spending_public_key(
        [seed; 32],
        [U256::from(seed), U256::from(seed + 1)],
    );
    let railgun_address = viewing.derive_address(None).expect("derive address");
    let address_data = AddressData::try_from(&RailgunAddress::from(railgun_address.as_ref()))
        .expect("address data");
    (
        PublicBroadcasterCandidate {
            chain_id: 1,
            railgun_address: railgun_address.to_string(),
            identifier: None,
            token: address(0x33),
            fee: uint!(10_U256),
            fees_id: "fees-id".to_string(),
            fee_expiration: SystemTime::now() + Duration::from_mins(1),
            reliability: 0.9,
            available_wallets: 1,
            version: "8.2.3".to_string(),
            relay_adapt: address(0x44),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            viewing_public_key: address_data.viewing_public_key,
            address_data,
            fee_policy_status: BroadcasterFeePolicyStatus::UnknownAnchor,
        },
        viewing,
    )
}

pub(super) fn sample_pre_tx_poi(byte: u8) -> PreTxPoi {
    PreTxPoi {
        snark_proof: SnarkJsProof {
            pi_a: [U256::from(byte), U256::from(byte + 1)],
            pi_b: [
                [U256::from(byte + 2), U256::from(byte + 3)],
                [U256::from(byte + 4), U256::from(byte + 5)],
            ],
            pi_c: [U256::from(byte + 6), U256::from(byte + 7)],
        },
        txid_merkleroot: FixedBytes::from([byte; 32]),
        poi_merkleroots: vec![FixedBytes::from([byte + 1; 32])],
        blinded_commitments_out: vec![FixedBytes::from([byte + 2; 32])],
        railgun_txid_if_has_unshield: Bytes::copy_from_slice(&[0_u8]),
    }
}

pub(super) fn sample_poi_map(
    list_keys: &[FixedBytes<32>],
    txid_leaf_hashes: &[FixedBytes<32>],
) -> BTreeMap<FixedBytes<32>, BTreeMap<FixedBytes<32>, PreTxPoi>> {
    list_keys
        .iter()
        .enumerate()
        .map(|(list_index, list_key)| {
            let per_leaf = txid_leaf_hashes
                .iter()
                .enumerate()
                .map(|(leaf_index, leaf)| {
                    (
                        *leaf,
                        sample_pre_tx_poi(10 + (list_index * 4 + leaf_index) as u8),
                    )
                })
                .collect();
            (*list_key, per_leaf)
        })
        .collect()
}

pub(super) fn sample_note(seed: u8, token: Address, value: u64) -> Note {
    Note::new_change(U256::from(seed), token, U256::from(value), [seed; 16])
}

pub(super) fn sample_chunk(
    tree_number: u32,
    bound_seed: u8,
    outputs: Vec<Note>,
    has_unshield: bool,
) -> TransactionPlanChunk {
    TransactionPlanChunk {
        tree_number,
        merkle_root: U256::from(bound_seed),
        inputs: Vec::new(),
        public_inputs: PublicInputs {
            merkle_root: U256::from(bound_seed),
            bound_params_hash: U256::from(bound_seed + 1),
            nullifiers: Vec::new(),
            commitments_out: outputs.iter().map(Note::commitment).collect(),
        },
        private_inputs: PrivateInputs {
            token_address: U256::from(bound_seed + 2),
            random_in: Vec::new(),
            value_in: Vec::new(),
            path_elements: Vec::new(),
            leaves_indices: Vec::new(),
            value_out: outputs.iter().map(|note| note.value).collect(),
            public_key: [U256::from(bound_seed + 3), U256::from(bound_seed + 4)],
            npk_out: outputs.iter().map(|note| note.npk).collect(),
            nullifying_key: U256::from(bound_seed + 5),
        },
        outputs,
        has_unshield,
        signature: [U256::ZERO; 3],
    }
}

pub(super) fn poi_map_for_chunks(
    list_keys: &[FixedBytes<32>],
    chunks: &[TransactionPlanChunk],
) -> BTreeMap<FixedBytes<32>, BTreeMap<FixedBytes<32>, PreTxPoi>> {
    let leaves = chunks
        .iter()
        .map(|chunk| {
            FixedBytes::from(
                railgun_txid_leaf_hash(chunk.railgun_txid(), u64::from(chunk.tree_number))
                    .to_be_bytes::<32>(),
            )
        })
        .collect::<Vec<_>>();
    sample_poi_map(list_keys, &leaves)
}

pub(super) fn fee_row(
    chain_id: u64,
    token: Address,
    fee: u64,
    reliability: f64,
    fees_id: &str,
) -> FeeRow {
    fee_row_with_broadcaster_seed(chain_id, token, fee, reliability, fees_id, 7)
}

pub(super) fn fee_row_with_broadcaster_seed(
    chain_id: u64,
    token: Address,
    fee: u64,
    reliability: f64,
    fees_id: &str,
    broadcaster_seed: u8,
) -> FeeRow {
    FeeRow {
        chain_id,
        railgun_address: Arc::from(sample_railgun_address(broadcaster_seed)),
        token_address: token,
        fee: U256::from(fee),
        signature_valid: true,
        fees_id: Arc::from(fees_id),
        fee_expiration: SystemTime::now() + Duration::from_mins(1),
        available_wallets: 1,
        version: Arc::from("8.2.3"),
        relay_adapt: address(0x44),
        relay_adapt_7702: None,
        required_poi_list_keys: Vec::new(),
        identifier: None,
        last_seen: SystemTime::now(),
        reliability,
    }
}

pub(super) fn broadcaster_preference_entry(seed: u8) -> vault::BroadcasterPreferenceEntry {
    vault::BroadcasterPreferenceEntry {
        address: sample_railgun_address(seed),
    }
}

pub(super) fn utxo_with_kind(
    token: Address,
    value: u64,
    tree: u32,
    position: u64,
    commitment_kind: UtxoCommitmentKind,
) -> WalletUtxo {
    let mut wallet_utxo = WalletUtxo::new(Utxo::new(
        Note::new_unshield(Address::ZERO, token, U256::from(value)),
        tree,
        position,
        source(position as u8 + 1),
        commitment_kind,
    ));
    for list_key in default_active_poi_list_keys() {
        wallet_utxo
            .utxo
            .poi
            .statuses
            .insert(list_key, PoiStatus::Valid);
    }
    wallet_utxo
}

pub(super) fn utxo(token: Address, value: u64, tree: u32, position: u64) -> WalletUtxo {
    utxo_with_kind(token, value, tree, position, UtxoCommitmentKind::Transact)
}

pub(super) fn blocked_shield_utxo(
    token: Address,
    value: u64,
    tree: u32,
    position: u64,
) -> WalletUtxo {
    let mut wallet_utxo = utxo_with_kind(token, value, tree, position, UtxoCommitmentKind::Shield);
    wallet_utxo
        .utxo
        .poi
        .statuses
        .insert(default_active_poi_list_keys()[0], PoiStatus::ShieldBlocked);
    wallet_utxo
}

pub(super) fn rescue_utxo_id(wallet_utxo: &WalletUtxo) -> BlockedShieldRescueUtxoId {
    BlockedShieldRescueUtxoId {
        tree: wallet_utxo.utxo.tree,
        position: wallet_utxo.utxo.position,
        commitment: wallet_utxo.utxo.poi.commitment,
        blinded_commitment: wallet_utxo.utxo.poi.blinded_commitment,
    }
}

pub(super) fn public_account(
    uuid: &str,
    address: Address,
    status: crate::vault::PublicAccountStatus,
) -> crate::vault::PublicAccountMetadata {
    crate::vault::PublicAccountMetadata {
        public_account_uuid: uuid.to_string(),
        address,
        label: Some(format!("Account {uuid}")),
        source: crate::vault::PublicAccountSource::Imported,
        scope: crate::vault::PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status,
        display_order: 0,
    }
}

pub(super) fn rescue_plan_for_test(
    utxo: &Utxo,
    token: Address,
    amount: U256,
    origin: Address,
    extra_input: Option<Utxo>,
    change_value: Option<U256>,
) -> UnshieldPlan {
    let mut inputs = vec![InputWitness {
        utxo: utxo.clone(),
        merkle_proof: MerkleProof {
            root: U256::ZERO,
            leaf: U256::ZERO,
            leaf_index: utxo.position,
            path_elements: [U256::ZERO; TREE_DEPTH],
            path_indices: [0; TREE_DEPTH],
        },
    }];
    if let Some(extra) = extra_input {
        inputs.push(InputWitness {
            utxo: extra,
            merkle_proof: MerkleProof {
                root: U256::ZERO,
                leaf: U256::ZERO,
                leaf_index: 0,
                path_elements: [U256::ZERO; TREE_DEPTH],
                path_indices: [0; TREE_DEPTH],
            },
        });
    }

    let unshield_note = Note::new_unshield(origin, token, amount);
    let change_note = change_value.map(|value| Note::new_change(U256::ZERO, token, value, [1; 16]));
    let mut outputs = Vec::new();
    if let Some(change) = change_note.clone() {
        outputs.push(change);
    }
    outputs.push(unshield_note.clone());
    let public_inputs = PublicInputs {
        merkle_root: U256::ZERO,
        bound_params_hash: U256::ZERO,
        nullifiers: Vec::new(),
        commitments_out: outputs.iter().map(Note::commitment).collect(),
    };
    let private_inputs = PrivateInputs {
        token_address: U256::from_be_slice(token.as_slice()),
        random_in: Vec::new(),
        value_in: Vec::new(),
        path_elements: Vec::new(),
        leaves_indices: Vec::new(),
        value_out: outputs.iter().map(|note| note.value).collect(),
        public_key: [U256::ZERO; 2],
        npk_out: outputs.iter().map(|note| note.npk).collect(),
        nullifying_key: U256::ZERO,
    };
    let chunk = TransactionPlanChunk {
        tree_number: utxo.tree,
        merkle_root: U256::ZERO,
        inputs: inputs.clone(),
        outputs: outputs.clone(),
        has_unshield: true,
        public_inputs: public_inputs.clone(),
        private_inputs: private_inputs.clone(),
        signature: [U256::ZERO; 3],
    };
    UnshieldPlan {
        call: TransactionCall {
            to: Address::ZERO,
            data: Bytes::new(),
        },
        tree_number: utxo.tree,
        merkle_root: U256::ZERO,
        inputs,
        outputs,
        chunks: vec![chunk],
        broadcaster_fee_note: None,
        unshield_note,
        unshield_notes: vec![Note::new_unshield(origin, token, amount)],
        change_note,
        public_inputs,
        private_inputs,
        signature: [U256::ZERO; 3],
    }
}

pub(super) fn spent_utxo(token: Address, value: u64, tree: u32, position: u64) -> WalletUtxo {
    let mut wallet_utxo = utxo(token, value, tree, position);
    wallet_utxo.spent = Some(source(9));
    wallet_utxo
}
