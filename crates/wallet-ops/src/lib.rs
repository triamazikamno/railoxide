use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use alloy::eips::BlockNumberOrTag;
use alloy::hex;
use alloy::network::{EthereumWallet, TransactionBuilder as _};
use alloy::primitives::{Address, Bytes, FixedBytes, U256, address, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{FeeHistory, TransactionRequest};
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;
use alloy::uint;
use broadcaster_core::contracts::shield::derive_shield_private_key;
use broadcaster_core::crypto::railgun::{Address as RailgunAddress, AddressData};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use broadcaster_core::transact::{
    BroadcasterRawParamsTransact, DEFAULT_TXID_VERSION, EncryptedTransactRequest,
    railgun_txid_leaf_hash,
};
use broadcaster_core::transact_response::DecryptedTransactResponse;
use broadcaster_monitor::FeeRow;
use eyre::{Report, Result, WrapErr, eyre};
use local_db::{
    DbConfig, DbStore, PendingOutputPoiContextRecord, PendingOutputPoiRole, WalletCacheKey,
    WalletMeta,
};
use merkletree::tree::MerkleForest;
pub use poi::SensitiveUrl;
use poi::poi::{PoiRpcClient, default_active_poi_list_keys};
use railgun_wallet::artifacts::ArtifactSource;
use railgun_wallet::prover::build_prover_cache_with_progress;
pub use railgun_wallet::prover::{
    ProverCacheBuildProgress, ProverCacheBuildReport, ProverCacheBuildStage,
};
use railgun_wallet::tx::{
    BroadcasterFeeOutput, BuildError, CompositePrivateOutputRole, CompositePrivateOutputRoleKind,
    CompositeRelayAction, CompositeRelayActionToken, CompositeRelayActions, CompositeUnshieldLeg,
    CompositeUnshieldLegRole, CompositeUnshieldPlan, CompositeUnshieldRecipient,
    CompositeUnshieldRequest, MAX_BATCH_TRANSACTIONS, MixedPrivateActionPlan,
    MixedPrivateActionRebuildConstraint, MixedPrivateActionRequest, MixedPrivateOutputRole,
    MixedPrivatePlannedOutput, MixedPrivateSend, MixedPrivateSendRole, MixedPublicPlannedOutput,
    PoiMerkleProofSource, PreTransactionPoiGenerationRequest, PreTransactionPoiMap,
    SelectedInputIdentity, SendPlan, SendRequest as RailgunSendRequest, TransactionPlanChunk,
    UnshieldMode, UnshieldPlan, UnshieldRequest as RailgunUnshieldRequest,
    generate_pre_transaction_pois, max_broadcaster_fee_token_spendable, max_send_spendable,
    max_unshield_spendable, send_selection_info, send_selection_info_with_broadcaster_fee_token,
    send_selection_info_with_separate_broadcaster_fee_seed, unshield_selection_info,
    unshield_selection_info_with_broadcaster_fee_token,
    unshield_selection_info_with_separate_broadcaster_fee_seed,
};
use railgun_wallet::{
    Note, PoiStatus, ProverService, TransactionBuilder, Utxo, UtxoCommitmentKind, UtxoPoiMetadata,
    UtxoSource, WalletUtxo,
};
use rand::seq::IndexedRandom;
use reqwest::Url;
use serde::Serialize;
use sync_service::types::WalletPpoiSubmissionStatus;
pub use sync_service::types::{
    WalletObservation, WalletPpoiWorkflowStatus, WalletReadiness, WalletReadinessError,
    WalletReadinessWaitError,
};
use sync_service::{
    ChainConfig, ChainConfigDefaults, ChainKey, LocalPoiMerkleProofSource,
    PendingOutputPoiContextIntent, PoiArtifactCacheRetry as CorePoiArtifactCacheRetry,
    PublicDataPlaneHandle, SyncManager, SyncProgressSender, WalletConfig, WalletHandle,
    WalletPendingOverlay, WalletPendingSpent, WalletPendingSpentMarkOutcome, WalletViewState,
};
pub use sync_service::{
    ChainPublicSyncCacheReset, ChainPublicSyncCacheResetResult, GlobalPoiPolicy as PoiReadSource,
    PersistedPublicSyncCacheKind, PersistedPublicSyncCacheResetError,
    PersistedPublicSyncCacheResetReport, PoiArtifactCacheAttemptId, PoiArtifactCacheFailureKind,
    PoiArtifactCacheGraphProgress, PoiArtifactCacheListProgress, PoiArtifactCachePhase,
    PoiArtifactCacheProgress, PoiArtifactManifestSource, PoiArtifactSourceConfig, PoiProxyFallback,
    PublicScanSource, PublicSyncCachesResetReport, SyncProgressStage, SyncProgressUnit,
    SyncProgressUpdate, WalletIndexedCatchUpSource, WalletIndexedCatchUpStatus,
    reset_persisted_public_sync_caches,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use waku_relay::client::Client as WakuClient;
use waku_relay::msg::ContentTopic;
use zeroize::{Zeroize, Zeroizing};

pub use local_db::DbStore as WalletDbStore;
pub use waku_relay::client::Client as WakuDeliveryClient;

static ACTIVE_PROVER_CACHE_BUILDS: LazyLock<
    Mutex<HashMap<PathBuf, watch::Sender<Option<ProverCacheBuildProgress>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

mod amounts;
mod anchors;
mod block_observer;
mod desktop;
mod governance;
mod governance_documents;
pub mod hardware;
mod hardware_typed_data;
mod http;
mod native_topup;
mod poi_contexts;
mod protocol_fee;
mod public_wallet;
mod signer;
mod sponsored_bundle;
mod sponsorship;
mod utxos;
pub mod walletconnect;

pub mod settings;
pub mod vault;

pub use amounts::{
    is_wrapped_native_token, parse_railgun_recipient, parse_send_amount, parse_unshield_amount,
};
pub use anchors::{
    BroadcasterFeePolicy, BroadcasterFeePolicyStatus, TokenAnchorRateCache,
    TokenAnchorRefreshHandle, average_non_outlier_anchor_rates, fixed_token_anchor_rate,
    known_token_anchor_sources, oracle_answer_to_anchor_rate, refresh_token_anchor_rates,
    spawn_token_anchor_refresh_worker,
};
pub use desktop::*;
pub use governance::{
    GovernanceContractRules, GovernanceContractSummary, GovernanceContractVersion,
    GovernanceMajorityResult, GovernanceOverview, GovernanceProposal, GovernanceProposalAction,
    GovernanceProposalDeadlines, GovernanceProposalStage, GovernanceProposalStatus,
    GovernanceQuorumBasis, derive_governance_proposal_stage, derive_governance_proposal_status,
    fetch_governance_chain_time, fetch_governance_overview, fetch_governance_page,
};
pub use governance_documents::{GovernanceDocument, resolve_governance_document};
pub use http::{
    HttpContext, TorBridgeActivitySnapshot, TorRuntimeHealth, WalletNetworkConfig,
    WalletNetworkHealth, WalletNetworkHealthCause, WalletNetworkHealthState, WalletNetworkMode,
    WalletNetworkProgress, WalletNetworkProgressStage, WalletTorClient, WalletTorClientProvider,
    build_http_client, build_wallet_network_context, build_wallet_network_context_with_progress,
    request_tor_state_reset, resolve_wallet_network_mode,
};
pub use native_topup::{
    DesktopNativeTopUpPlan, DesktopNativeTopUpRequest, NATIVE_TOP_UP_ARBITRUM_AMOUNT,
    NATIVE_TOP_UP_BSC_AMOUNT, NATIVE_TOP_UP_ETHEREUM_AMOUNT, NATIVE_TOP_UP_POLYGON_AMOUNT,
    NativeTopUpPolicy, native_top_up_policy_for_chain,
    native_top_up_primary_recipient_amount_for_fee_mode,
    native_top_up_required_wrapped_native_amount,
    native_top_up_required_wrapped_native_amount_for_fee_mode, native_top_up_wrapped_native_amount,
    sponsored_unshield_additional_wrapped_native_spend,
};
pub(crate) use native_topup::{
    native_top_up_net_after_protocol_fee, native_top_up_wrapped_native_amount_for_net,
};
pub(crate) use protocol_fee::{FEE_BASIS_POINTS_DENOMINATOR, railgun_protocol_fee_amount};
pub use protocol_fee::{RAILGUN_PROTOCOL_FEE_BPS, format_protocol_fee_percentage};
pub use public_wallet::{
    HardwareTrezorPinMatrixProvider, PUBLIC_NATIVE_UNWRAP_GAS_UNITS, PublicAccountBalance,
    PublicActionAttemptInfo, PublicActionCommand, PublicActionCommandKind,
    PublicActionCommandReceiver, PublicActionCommandSender, PublicActionFeeProjection,
    PublicActionFeeSource, PublicActionGasFeeMode, PublicActionGasFeeQuote,
    PublicActionGasFeeQuoteBundle, PublicActionGasFeeSelection, PublicActionKind,
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionResolvedGasFee, PublicActionSessionEvent, PublicActionSessionEventSender,
    PublicActionStepFeePolicy, PublicAdvancedTransactionAuthorization,
    PublicAdvancedTransactionEstimate, PublicAdvancedTransactionEstimateRequest,
    PublicAdvancedTransactionSimulationError, PublicAssetId, PublicBalanceAmount,
    PublicBalanceAsset, PublicBalanceEntry, PublicBalanceRefreshCoordinator, PublicBalanceSnapshot,
    PublicSendRequest, PublicSendResult, PublicShieldRequest, PublicShieldTransactionProfile,
    PublicTransactionIntent, WalletConnectHardwareTypedDataCapabilityRequest,
    WalletConnectHardwareTypedDataCapabilityResult,
    WalletConnectHardwareTypedDataHashFallbackConfirmationRequired,
    WalletConnectPersonalSignRequest, WalletConnectReviewedFee, WalletConnectReviewedTransaction,
    WalletConnectSendTransactionRequest, WalletConnectSendTransactionResult,
    WalletConnectTypedDataSignRequest, estimate_public_action_gas_cost,
    estimate_public_action_gas_cost_with_profile,
    estimate_public_action_gas_cost_with_profile_and_ceiling, estimate_public_advanced_transaction,
    estimate_public_advanced_transaction_with_fee, estimate_public_native_action_gas_reserve,
    estimate_public_native_action_gas_reserve_with_profile,
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling,
    is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required,
    project_public_action_fee, public_action_maximum_gas_cost_is_significant,
    public_action_replacement_bumped_fee, public_balance_assets_for_chain,
    public_balance_refresh_interval_secs, public_native_action_gas_reserve,
    public_native_action_gas_units, public_native_action_gas_units_from_walletconnect_intent,
    public_shield_protocol_fee_amount, public_walletconnect_operation_gas_limit,
    quote_public_action_gas_fee, quote_public_action_gas_fee_bundle_with_profile,
    quote_public_action_gas_fee_with_profile, refresh_public_balances,
    resolve_public_action_gas_fee, sanitize_walletconnect_transaction_request,
    simulate_public_advanced_transaction_with_fee, submit_public_send,
    submit_public_send_with_progress, submit_public_shield, submit_public_shield_with_progress,
    submit_walletconnect_send_transaction, validate_walletconnect_reviewed_transaction,
    walletconnect_hardware_typed_data_hash_fallback_confirmation_session,
    walletconnect_probe_hardware_typed_data_signing_mode, walletconnect_sign_personal_message,
    walletconnect_sign_typed_data, walletconnect_sign_typed_data_v4,
    walletconnect_transaction_payload_fingerprint,
};
use public_wallet::{VaultedPublicSigner, vaulted_public_signer};
pub use sponsored_bundle::*;
pub use sponsorship::*;
use utxos::apply_pending_overlay_to_outputs;
pub use utxos::{
    ActivityUtxoClassification, BlockedShieldRescueInfo, ListUtxosOutput, TokenTotal, UtxoOutput,
    UtxoPpoiState, max_broadcaster_fee_token_amount_from_outputs, max_send_amount_from_outputs,
    max_unshield_amount_from_outputs,
};
pub use walletconnect::{
    WALLETCONNECT_DEFAULT_PROJECT_ID, WALLETCONNECT_EIP155_NAMESPACE,
    WALLETCONNECT_IRN_RELAY_PROTOCOL, WALLETCONNECT_RELAY_RPC_URL,
    WALLETCONNECT_REQUIRED_PAIRING_METHOD, WC_SESSION_PROPOSE, WC_SESSION_PROPOSE_RESPONSE_TAG,
    WC_SESSION_SETTLE, WC_SESSION_SETTLE_REQUEST_TAG, WalletConnectApprovalMessages,
    WalletConnectDecodedCallKind, WalletConnectDecodedTransaction, WalletConnectDisconnectPlan,
    WalletConnectEnvelope, WalletConnectError, WalletConnectEvmTransaction,
    WalletConnectJsonRpcRequest, WalletConnectJsonRpcResponse,
    WalletConnectLifecycleRequestOutcome, WalletConnectNamespaceAccountSupport,
    WalletConnectNamespaceNegotiation, WalletConnectNamespaceProposal, WalletConnectPairingStart,
    WalletConnectPairingUri, WalletConnectParsedRequest, WalletConnectPendingRequest,
    WalletConnectPendingRequestQueue, WalletConnectProposalRejectionReason,
    WalletConnectProposalSummary, WalletConnectRelayClient, WalletConnectRelayClientAuth,
    WalletConnectRelayConfig, WalletConnectRelayLifecycle, WalletConnectRelayRpc,
    WalletConnectRelaySocket, WalletConnectRelayStep, WalletConnectRelaySubscriptionPayload,
    WalletConnectRequestErrorKind, WalletConnectRequestValidation, WalletConnectSessionApproval,
    WalletConnectSessionProposal, WalletConnectSessionRequest, WalletConnectSupportedEvent,
    WalletConnectSupportedMethod, WalletConnectTerminalLifecycleEnd,
    WalletConnectTransactionRequest, WalletConnectUnsupportedNamespaceItem,
    approve_walletconnect_session, approve_walletconnect_session_with_account_support,
    build_walletconnect_disconnect_plan, build_walletconnect_jsonrpc_error,
    build_walletconnect_session_event, decode_walletconnect_message,
    decode_walletconnect_session_proposal, derive_walletconnect_session_sym_key,
    derive_walletconnect_session_topic, encode_walletconnect_message,
    generate_walletconnect_key_pair, handle_walletconnect_lifecycle_request,
    hash_walletconnect_key, negotiate_walletconnect_namespaces,
    negotiate_walletconnect_namespaces_with_account_support, parse_walletconnect_session_request,
    reject_walletconnect_session_proposal, start_walletconnect_pairing,
    validate_walletconnect_session_request,
    validate_walletconnect_session_request_with_account_support,
};

#[cfg(test)]
mod tests;
