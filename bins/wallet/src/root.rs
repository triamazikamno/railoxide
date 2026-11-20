use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use broadcaster_monitor::{EventRx, EventTx, Shared};
#[cfg(test)]
use broadcaster_monitor_waku::WakuMonitorConfig;
#[cfg(test)]
use gpui::Focusable;
use gpui::{AppContext, Context, Entity, FocusHandle, Pixels, SharedString, Window, px};
#[cfg(test)]
use gpui_component::WindowExt;
use gpui_component::{
    input::InputState,
    select::{SearchableVec, SelectState},
    table::TableState,
};
use rand::RngExt;
use tokio::runtime::Handle;
use tokio::sync::watch;
// The header's network. EVM-only code reaches a chain id only by matching `Evm`.
use ui::chain_select::Network as SelectedNetwork;
use ui::logs::LogsPane;
use ui::theme::APP_TEXT_SIZE;
use wallet_ops::{
    BlockedShieldRescueUtxoId, BroadcasterFeePolicy, HttpContext, PoiArtifactCacheProgress,
    PoiReadSource, PublicBalanceCache, PublicBalanceSnapshot, SponsoredSelfBroadcastCommand,
    SubmissionNodeScope, TokenAnchorRateCache, TokenAnchorRefreshHandle,
    settings::{
        EffectiveChainRegistry, EffectiveTokenRegistry, WalletUiState, load_wallet_settings,
        save_wallet_ui_state,
    },
    subscribe_activity_journal_failures,
    vault::{
        DesktopVaultStore, DesktopViewSession, DeviceAuthStatus, GeneratedSeedMaterial,
        NonEvmAddressBookEntry, PrivateAddressBookEntry, ProtectedSoftwareSeedSession,
        PublicAccountMetadata, PublicAddressBookEntry, ViewUnlock, WalletMetadataBundle,
    },
};
use zeroize::Zeroizing;

mod actions;
mod address_book;
mod auto_lock;
mod broadcaster_picker;
mod broadcaster_preferences;
mod broadcaster_view;
mod chain_load;
mod dapp_request;
mod device_auth;
mod dialogs;
mod gas_fee;
mod gateway;
mod gateway_drafts;
mod gateway_private_view;
mod gateway_public_view;
mod governance;
mod governance_action;
mod key_export;
mod locked_notes;
mod logs_search;
mod maintenance;
mod manage_wallets;
mod markdown_sanitize;
mod network;
mod participant;
mod platform_attention;
mod private_action;
mod private_assets;
mod private_broadcaster;
mod private_swap;
mod proposals;
mod prover_cache_monitor;
mod public_account;
mod public_action;
mod public_balances;
mod public_broadcaster;
mod public_broadcaster_cost;
mod public_transactions;
mod retry;
mod search_field;
mod settings;
mod shell;
mod sidebar;
mod spend_authorization;
mod startup;
mod status_bar;
mod stealth_accounts;
mod submission_progress;
mod submission_route;
mod tokens;
mod ui_helpers;
mod update_check;
mod utxo;
mod vault;
mod vault_ui;
mod waku_runtime;
mod wallet_activity;
mod wallet_header;
mod walletconnect;
mod widgets;
mod wiring;

#[cfg(test)]
mod tests;

pub(crate) use actions::{install_utxo_navigation_bindings, install_wallet_action_bindings};
pub(crate) use shell::{WalletAppOptions, open_wallet_window};

use address_book::AddressBookState;
#[cfg(test)]
use auto_lock::{AutoLockDeadlineStatus, apply_initial_sync_observation};
use auto_lock::{
    AutoLockState, InitialCatchUpFingerprint, InitialSyncActivity, InitialSyncObservation,
};
use broadcaster_picker::BroadcasterPickerState;
use broadcaster_preferences::{BroadcasterPreferencesUi, install_broadcaster_preference_snapshot};
use broadcaster_view::BroadcasterActivityTab;
use chain_load::{
    ChainUtxoState, WalletSyncLifecycle, WalletSyncLifecycleCleanupTask, chain_load_overrides,
};
use device_auth::{DeviceAuthStartup, DeviceAuthUi};
use gas_fee::Eip1559GasFeeEditorState;
use governance::GovernanceState;
use key_export::KeyExportState;
use maintenance::WalletMaintenanceController;
use manage_wallets::ManageWalletsState;
use network::NetworkStatusUi;
use private_action::{
    DeliveryFormKind, DeliveryMode, PrivateActionFormState, SendFormState, UnshieldAsset,
    UnshieldAssetKey, UnshieldFormState,
};
use private_broadcaster::PrivateBroadcasterProgressState;
use proposals::ProposalsState;
use prover_cache_monitor::ProverCacheBuildUi;
use public_account::{PublicAccountFormState, TronPublicState, TronSendState};
use public_action::PublicActionMode;
#[cfg(test)]
use public_action::PublicSendKind;
use public_balances::{
    public_account_visible_balances_for_chain, public_asset_decimals, public_asset_label,
    public_balance_amount_label,
};
use public_broadcaster::{
    PublicBroadcasterFeeTokenOption, broadcaster_candidate_anchor_rate,
    effective_fee_handling_mode, ethereum_weth_public_broadcaster_count,
    public_broadcaster_fee_token_warning, public_broadcaster_submit_disabled_for_fee_token_options,
    send_form_max_entered_amount, should_show_distinct_amount, should_show_fee_mode_toggle,
    unshield_form_max_entered_amount, unshield_max_entered_amount_for_mode,
    unshield_max_fill_amount,
};
use settings::WalletSettingsEditor;
use shell::{PoiArtifactCacheRetryAttempts, WalletTab};
#[cfg(test)]
use shell::{ppoi_hover_detail, ppoi_hover_heading};
use sidebar::Activity;
use spend_authorization::{SpendAuthorizationCache, SpendAuthorizationLifetime};
use startup::{WalletStartupReady, WalletStartupRoot};
use status_bar::LogsPanelState;
use tokens::{
    format_exact_token_amount_for_display, format_native_token_amount_ceiling_for_display,
    format_native_token_amount_for_display, format_native_top_up_recipient_suffix,
    format_recipient_amount_with_native_top_up, format_send_amount_input,
    format_token_amount_ceiling_for_display, format_token_amount_for_display,
    format_unshield_amount_input, format_value_with_usd_label, is_effective_wrapped_native_token,
    native_token_display_label, native_wrapped_output_labels, parse_address, token_display_label,
    token_display_metadata,
};
use ui_helpers::{
    ConfirmationDialogProps, app_panel, app_refresh_button, app_status_tag, app_step_row,
    app_stepper_container, centered_message, confirmation_dialog, copyable_mono_field, count_label,
    dialog_content_max_height, dialog_max_height, labeled_field, rgb_with_alpha,
    secondary_dialog_content_width, token_label_row,
};
use utxo::{BlockedShieldRescueRowState, UtxoDelegate, should_focus_utxo_table};
use vault::{
    PassphraseOpenUi, PendingSoftwareProfileOpen, VaultState, WalletOption, WalletSetupMode,
    vault_error_kind,
};
use waku_runtime::{
    WakuState, WakuStoppingState, WakuWorkerCompletionToken, WalletWakuRuntime,
    release_retired_waku_generation, renew_submission_scope,
};
use wallet_header::WalletSelectItem;
use widgets::WalletRootWidgets;

#[cfg(test)]
use broadcaster_picker::{
    BroadcasterChoice, BroadcasterPickerFeeEstimateRetryState, BroadcasterPickerFeeStatus,
    BroadcasterPickerTier, BroadcasterPickerViewMode,
    broadcaster_candidate_estimated_fee_amount_for_estimate,
    broadcaster_choice_supported_by_candidates, broadcaster_picker_fee_status,
    broadcaster_picker_fee_status_detail, broadcaster_picker_scroll_hint_visible,
    should_preserve_estimate_after_broadcaster_policy_change,
};
#[cfg(test)]
use chain_load::{
    BalanceSyncIssue, PresenceStatus, WalletStatusCounts, balance_lag_threshold_blocks,
    balance_stale_timeout, balance_sync_issue, balances_presence_status, loading_summary,
    ppoi_presence_status, progress_detail, ready_wallet_status_labels,
    ready_wallet_status_shows_text, wallet_generation_matches,
};
#[cfg(test)]
use gas_fee::{format_gwei, parse_gwei_to_wei, validate_custom_gas_fee};
#[cfg(test)]
use manage_wallets::{
    WalletManagementSelection, active_wallet_management_rows, hidden_wallet_management_rows,
    selected_wallet_after_metadata_refresh, wallet_ids_after_drop, wallet_source_label,
};
#[cfg(test)]
use private_action::native_top_up_request_from_plan;
#[cfg(test)]
use private_action::{
    PrivateWalletRecipientSource, RecipientOption, RecipientOptionSource,
    SEND_AUTHORIZATION_FAILED_ERROR, SelfBroadcastNativeBalanceState,
    UNSHIELD_AUTHORIZATION_FAILED_ERROR, adjusted_amount_for_max_change,
    can_save_private_recipient, can_save_public_recipient, default_self_broadcast_gas_payer_uuid,
    form_error_clears_public_broadcaster_cost_estimate, format_exact_asset_amount_for_display,
    format_form_error_for_asset, normalized_address_book_save_label,
    private_action_assets_from_snapshot, private_action_metric_display_amount,
    private_send_recipient_options, private_unshield_recipient_options,
    random_self_broadcast_gas_payer_uuid, recipient_option_matches_search,
    selected_recipient_address, self_broadcast_gas_payer_matches_search,
    self_broadcast_native_balance_label, self_broadcast_native_balance_state,
    self_broadcast_requires_software_gas_payer_password, send_element_id,
    send_public_broadcaster_estimate_input_error, unshield_element_id,
    unshield_public_broadcaster_estimate_input_error,
};
#[cfg(test)]
use private_assets::{
    format_private_asset_rows, format_total, max_send_amount_from_snapshot,
    max_unshield_amount_from_snapshot, pending_shield_wait_matches_total,
    pending_shield_waits_by_token, private_asset_display_amounts, refresh_form_asset_from_snapshot,
    retry_poi_label, send_asset_key_from_formatted, send_key_matches_asset,
    unshield_asset_key_from_formatted, unshield_key_matches_asset,
};
#[cfg(test)]
use private_broadcaster::{
    apply_private_broadcaster_progress_stage, ensure_self_broadcast_unshield_progress_stage,
    fail_private_broadcaster_progress_steps_at_stage, finish_private_broadcaster_progress_steps,
    finish_private_broadcaster_progress_steps_at_stage,
    finish_private_self_broadcast_progress_steps_at_stage,
    format_public_broadcaster_wait_remaining, mark_private_broadcaster_active_step_stopped,
    private_broadcaster_progress_footer_action, private_broadcaster_progress_steps,
    private_progress_stage_disables_stop, public_broadcaster_wait_status_detail,
    self_broadcast_progress_steps,
};
#[cfg(test)]
use public_account::{
    PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT, PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE,
    PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES, next_public_account_label_number,
    public_account_identicon_color, public_account_identicon_pattern,
    public_account_matches_search, public_address_qr_module_range, public_address_qr_payload,
};
#[cfg(test)]
use public_action::{
    AdvancedPublicSendField, ProgressFooterAction, PublicActionStepState, PublicActionStepStatus,
    advanced_public_send_review_metadata, advanced_public_send_warnings,
    authorized_public_action_gas_fee_selection, format_advanced_data_length, format_gas_limit,
    mark_public_action_active_step_stopped, parse_advanced_public_send_intent,
    progress_footer_action, public_action_accepts_update, public_action_closed_active_step,
    public_action_error_copy_value, public_action_error_details, public_action_error_summary,
    public_action_max_amount_after_reserve, public_action_progress_footer_action,
    public_action_step_color, public_action_step_detail, public_action_step_is_final_handoff,
    public_action_step_is_final_handoff_for_steps, public_action_step_uses_stop_marker,
    public_action_uses_railway_authorization_ceiling,
};
#[cfg(test)]
use public_balances::{public_asset_icon_path, public_balance_usd_label};
#[cfg(test)]
use public_broadcaster::{
    fee_token_option_has_eligible_broadcaster, highest_value_public_broadcaster_fee_token,
    public_broadcaster_fee_token_options_from_snapshot,
    resolve_selected_public_broadcaster_fee_token,
};
#[cfg(test)]
use public_broadcaster_cost::{
    CostEstimateStatus, PublicBroadcasterCostDisplay, format_public_broadcaster_fee_margin,
    public_broadcaster_cost_status_text, should_render_public_broadcaster_cost_preview,
};
#[cfg(test)]
use settings::{
    PriceAnchorComponentDialogValues, PriceAnchorDialogValues, SettingsApplyMode,
    StartupSettingsActionState, WakuPeerList, add_poi_gateway_url, add_waku_dns_enr_tree,
    add_waku_doh_fallback_endpoint, auto_lock_timeout_from_value, auto_lock_timeout_options,
    auto_lock_timeout_value, classify_settings_apply_mode, display_price_anchor_entries,
    display_token_entries, display_waku_dns_enr_trees, display_waku_doh_endpoint,
    display_waku_doh_fallback_endpoints, format_anchor_bps_exact_range, format_anchor_bps_percent,
    format_anchor_bps_percent_range, format_anchor_premium_range,
    price_anchor_dialog_values_from_entry, price_anchor_override_from_dialog_values,
    price_anchor_token_primary_label, proxy_url_resolves_dns_locally, remove_poi_gateway_url,
    remove_waku_dns_enr_tree, remove_waku_doh_fallback_endpoint, set_poi_gateway_url,
    set_price_anchor_override, set_waku_dns_enr_tree, set_waku_doh_fallback_endpoint,
    settings_draft_after_discard, settings_restart_action_enabled,
    settings_restart_reuses_active_network, settings_save_action_enabled,
    should_show_proxy_url_setting, should_show_proxy_waku_disclaimer,
    startup_settings_action_state,
};
#[cfg(test)]
use spend_authorization::{
    SpendAuthorizationSummary, is_spend_authorization_failure_error,
    remembered_spend_authorization_valid_for_test, spend_authorization_can_use_cached_password,
};
#[cfg(test)]
use startup::{load_validated_startup_settings, resolve_initial_chain_id};
#[cfg(test)]
use utxo::{
    UtxoDisplayRow, UtxoFinalityContext, activity_classification_icon_style,
    apply_blocked_shield_rescue_rows, blocked_shield_refund_action_available,
    blocked_shield_refund_origin_resolving, display_rows_from_output, global_poi_retry_available,
    pending_finality_display, poi_retry_button_label, ppoi_row_retry_label, ppoi_state_detail,
    recoverable_poi_candidate_count, shield_poi_wait_display, should_refresh_utxo_ages,
    should_show_blocked_shield_refund_action, should_show_ppoi_retry_action,
};
#[cfg(test)]
use vault::{
    HARDWARE_PROFILE_ADD_SUBACCOUNT_BUTTON_ID, HARDWARE_PROFILE_RECOVER_EXACT_BUTTON_ID,
    HARDWARE_PROFILE_RECOVER_RANGE_BUTTON_ID, default_hardware_wallet_setup_intent,
    hardware_profile_label_warning, hardware_wallet_creation_result_is_current,
    parse_hardware_exact_recovery_index, parse_hardware_recovery_range,
    parse_hardware_wallet_restore_account_index, should_focus_vault_input,
    trezor_passphrase_mode_copy, wallet_options_from_metadata,
};
#[cfg(all(test, feature = "hardware"))]
use vault::{
    HardwareAccountPickerRow, HardwareProfileStep, HardwareProfileStepStatus,
    HardwareProfileUnlockPurpose, HardwareProfileUnlockState,
    dismiss_hardware_profile_unlock_state, hardware_profile_auto_open_wallet_id,
};
#[cfg(test)]
use vault_ui::should_show_pre_unlock_settings_action;
#[cfg(test)]
use waku_runtime::{
    WakuWorkerCompletionAction, WakuWorkerCompletionKind, WakuWorkerQuiescenceGuard,
    active_waku_client, build_waku_client_if_needed, complete_waku_worker_generation,
    run_waku_worker_task, should_start_waku_for_delivery, should_start_waku_runtime,
    stop_waku_runtime, waku_start_blocked,
};
#[cfg(test)]
use wallet_header::{parse_repair_cache_block, repair_cache_help_text};
#[cfg(test)]
use wallet_ops::{PublicBroadcasterWaku, public_broadcaster_candidates_for_asset};
#[cfg(test)]
use walletconnect::{
    normalized_walletconnect_account_uuid, walletconnect_account_matches_search,
    walletconnect_account_select_items,
};

const SIDEBAR_WIDTH: Pixels = px(220.0);
const SIDEBAR_AUTO_COLLAPSE_WIDTH: Pixels = px(900.0);
const LOGS_DRAWER_HEIGHT: Pixels = px(260.0);
const LOGS_DRAWER_MIN_HEIGHT: Pixels = px(160.0);
const LOGS_DRAWER_MAX_HEIGHT: Pixels = px(600.0);
const PRIVATE_ASSET_LIST_WIDTH: Pixels = px(760.0);
const PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH: Pixels = px(560.0);
const PUBLIC_ACCOUNT_DIALOG_WIDTH: Pixels = px(460.0);
const PUBLIC_ADDRESS_QR_DIALOG_WIDTH: Pixels = px(440.0);
const PUBLIC_ACTION_DIALOG_WIDTH: Pixels = px(520.0);
const HERO_STAGE_MAX_WIDTH: Pixels = px(1440.0);
const HERO_WIDE_BREAKPOINT: Pixels = px(1280.0);
const HERO_MEDIUM_BREAKPOINT: Pixels = px(720.0);
const HERO_CARD_MAX_WIDTH: Pixels = px(520.0);
const NETWORK_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const TOR_HEALTH_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const TOR_EXIT_IP_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const TOR_EXIT_IP_QUERY_URL: &str = "https://check.torproject.org/api/ip";
const UNSHIELD_SPINNER_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const UTXO_AGE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const COST_ESTIMATE_DEBOUNCE: Duration = Duration::from_secs(1);
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const TABLE_KEY_CONTEXT: &str = "Table";

pub(super) const fn should_apply_background_focus(has_active_dialog: bool) -> bool {
    !has_active_dialog
}

pub(crate) struct WalletRoot {
    options: WalletAppOptions,
    vault_store: Option<Arc<DesktopVaultStore>>,
    poi_read_source: PoiReadSource,
    effective_chain_configs: EffectiveChainRegistry,
    effective_token_registry: EffectiveTokenRegistry,
    public_balance_refresh_interval: Duration,
    auto_lock: AutoLockState,
    initial_sync_activity: InitialSyncActivity,
    public_broadcaster_policy: BroadcasterFeePolicy,
    public_broadcaster_sort_seed: [u8; 32],
    public_broadcaster_response_timeout: Duration,
    public_broadcaster_republish_interval: Duration,
    default_allow_suspicious_broadcasters: bool,
    mimic_railway_shields_by_default: bool,
    unwrap_unshields_by_default: bool,
    vault_state: VaultState,
    wallet_setup_mode: WalletSetupMode,
    vault_error: Option<Arc<str>>,
    unlock_in_progress: bool,
    repair_cache_error: Option<Arc<str>>,
    setup_password: Option<Zeroizing<String>>,
    vault_view_unlock: Option<Arc<ViewUnlock>>,
    spend_authorization_cache: Option<SpendAuthorizationCache>,
    spend_authorization_lifetime: SpendAuthorizationLifetime,
    protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
    pending_software_profile_open: Option<PendingSoftwareProfileOpen>,
    passphrase_open_ui: Entity<PassphraseOpenUi>,
    pending_software_profile_open_operation_generation: u64,
    pending_software_profile_open_lifetime_generation: u64,
    pending_software_profile_base_profile_uuid: Option<Arc<str>>,
    revealed_passphrase_context_id: Option<Arc<str>>,
    view_session: Option<Arc<DesktopViewSession>>,
    generated_seed: Option<GeneratedSeedMaterial>,
    generated_seed_revealed: bool,
    hardware_wallet_creation: vault::HardwareWalletCreation,
    #[cfg(feature = "hardware")]
    hardware_profile_unlock: vault::HardwareProfileUnlockState,
    #[cfg(feature = "hardware")]
    hardware_profile: vault::HardwareProfileUi,
    http: HttpContext,
    /// Identity warmup for the installed routes on `http`; replaced with the settings and
    /// cleared before the network context is handed off.
    rpc_identity_warmup: Option<wallet_ops::RpcIdentityWarmup>,
    active_rpc_chains: BTreeSet<u64>,
    network_context_id: String,
    network_status: NetworkStatusUi,
    root_shutdown: watch::Sender<bool>,
    gateway: gateway::GatewayUi,
    prover_cache_build: ProverCacheBuildUi,
    runtime: Handle,
    monitor_state: Shared,
    monitor_event_tx: EventTx,
    waku: WakuState,
    /// Ends public broadcaster submission nodes on lock and root shutdown.
    submission_scope: SubmissionNodeScope,
    public_broadcaster_anchor_cache: Arc<TokenAnchorRateCache>,
    public_broadcaster_anchor_refresh: TokenAnchorRefreshHandle,
    monitor: Entity<broadcaster_monitor_gpui::BroadcasterMonitorPane>,
    logs_panel: LogsPanelState,
    settings_editor: Option<Entity<WalletSettingsEditor>>,
    maintenance_controller: Entity<WalletMaintenanceController>,
    settings_error: Option<Arc<str>>,
    /// The last Open failure of the data folder row shown while Settings are unavailable.
    data_folder_open_error: Option<SharedString>,
    active_activity: Activity,
    active_wallet_tab: WalletTab,
    sidebar_manually_collapsed: bool,
    sidebar_narrow_expanded: bool,
    sidebar_public_broadcaster_count: usize,
    wallet_select: Entity<SelectState<SearchableVec<WalletSelectItem>>>,
    wallet_metadata: Vec<WalletMetadataBundle>,
    wallet_options: Vec<WalletOption>,
    manage_wallets: ManageWalletsState,
    key_export: KeyExportState,
    update_check: update_check::UpdateCheckUi,
    manage_wallet_label_input: Entity<InputState>,
    selected_wallet_id: Option<Arc<str>>,
    active_wallet_generation: u64,
    wallet_switch_generation: u64,
    wallet_switch_loading_generation: Option<u64>,
    wallet_switch_delayed: bool,
    selected_network: SelectedNetwork,
    /// The most recently selected EVM chain. Background chain work and the gateway follow it,
    /// so selecting Tron leaves them running.
    last_evm_chain: u64,
    ui_state: WalletUiState,
    chain_select: Entity<SelectState<wallet_header::NetworkSelectItems>>,
    window_handle: gpui::AnyWindowHandle,
    chain_states: BTreeMap<u64, ChainUtxoState>,
    pending_ppoi_validation_toast: Option<(Arc<str>, u64)>,
    /// Failed Activity journal writes, polled by the root tick for the lifetime of the root.
    activity_journal_failures: watch::Receiver<u64>,
    pending_activity_journal_notice: bool,
    private_pending_status_dialog_open: bool,
    private_asset_presentation_cache: RefCell<private_assets::PrivateAssetPresentationCache>,
    poi_artifact_cache_progress: BTreeMap<u64, PoiArtifactCacheProgress>,
    poi_artifact_cache_retry_attempts: PoiArtifactCacheRetryAttempts,
    wallet_sync_lifecycle: WalletSyncLifecycle,
    wallet_sync_cleanup_tasks: Vec<WalletSyncLifecycleCleanupTask>,
    wallet_sync_lifecycle_shutdown_started: bool,
    public_sync_cache_resetting: bool,
    merkle_forest_cache_resetting: bool,
    vault_inputs: vault::VaultInputs,
    public_accounts: Vec<PublicAccountMetadata>,
    address_book: AddressBookState,
    governance: GovernanceState,
    proposals: ProposalsState,
    proposal_detail_focus: FocusHandle,
    private_address_book: Vec<PrivateAddressBookEntry>,
    public_address_book: Vec<PublicAddressBookEntry>,
    non_evm_address_book: Vec<NonEvmAddressBookEntry>,
    broadcaster_preferences: BroadcasterPreferencesUi,
    address_book_label_input: Entity<InputState>,
    address_book_save_error: Option<Arc<str>>,
    public_form: PublicAccountFormState,
    stealth_accounts: Option<stealth_accounts::StealthAccountsPanel>,
    private_swaps: Option<private_swap::PrivateSwapsPanel>,
    /// The Activity tab's journal data, rows and clear dialog for the selected wallet.
    wallet_activity: wallet_activity::WalletActivityState,
    public_balance_cache: PublicBalanceCache,
    /// Tron accounts and balances, and Tron's route.
    tron_public: TronPublicState,
    public_transaction_tracker: wallet_ops::PublicTransactionTracker,
    public_transaction_submissions: public_transactions::PublicTransactionSubmissions,
    public_transaction_cleanup: Option<public_transactions::PublicTransactionCleanup>,
    public_balance_snapshot: Option<Arc<PublicBalanceSnapshot>>,
    public_balance_error: Option<Arc<str>>,
    public_inactive_balance_error: Option<Arc<str>>,
    send_forms: BTreeMap<UnshieldAssetKey, SendFormState>,
    private_action_form: Option<PrivateActionFormState>,
    send_generation_seq: u64,
    unshield_generation_seq: u64,
    cost_estimate_seq: u64,
    unshield_forms: BTreeMap<UnshieldAssetKey, UnshieldFormState>,
    private_broadcaster_progress: Option<PrivateBroadcasterProgressState>,
    public_broadcaster_task_abort_handles: Vec<tokio::task::AbortHandle>,
    broadcaster_picker: Option<BroadcasterPickerState>,
    unshield_spinner_tick: usize,
    repair_cache_block_input: Entity<InputState>,
    tx_search_input: Entity<InputState>,
    tx_search_query: Arc<str>,
    platform_attention_count: usize,
    platform_attention_window_active: bool,
    spend_authorization_attention: Vec<std::rc::Weak<()>>,
    platform_attention: platform_attention::PlatformAttentionState,
    walletconnect: walletconnect::WalletConnectUiState,
    show_spent_utxos: bool,
    local_pending_spent_clear_confirming: bool,
    blocked_shield_rescue_lookup_generation: u64,
    blocked_shield_rescue_rows: BTreeMap<BlockedShieldRescueUtxoId, BlockedShieldRescueRowState>,
    blocked_shield_refunds_in_flight: BTreeSet<BlockedShieldRescueUtxoId>,
    /// Notes stealth-account operations lock on the selected chain, as of the last UTXO
    /// table sync. Rendering reads this instead of decrypting executor records.
    executor_locked_note_count: usize,
    utxo_table: Entity<TableState<UtxoDelegate>>,
    focus_vault_input_on_render: bool,
    device_auth: DeviceAuthUi,
    /// Weak identity of the active add-wallet dialog; its builder owns the lease.
    add_wallet_dialog_lease: std::rc::Weak<Cell<bool>>,
    /// The vault password device authentication supplied for the add-wallet form, if any.
    add_wallet_device_auth_password: Option<Zeroizing<String>>,
    focus_utxo_table_on_render: bool,
    focus_public_account_search_on_render: bool,
    wallet_focus: FocusHandle,
}

impl Drop for WalletRoot {
    fn drop(&mut self) {
        self.begin_public_transaction_shutdown();
        self.submission_scope.close();
        self.pending_software_profile_open = None;
        self.pending_software_profile_open_operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_lifetime_generation = self
            .pending_software_profile_open_lifetime_generation
            .wrapping_add(1);
        self.protected_software_seed_session = None;
        self.spend_authorization_cache = None;
        if let Some(command_tx) = self
            .private_broadcaster_progress
            .as_ref()
            .and_then(|progress| progress.sponsored_self_broadcast_command_tx.as_ref())
        {
            let _ = command_tx.send(SponsoredSelfBroadcastCommand::Shutdown);
        }
        self.stop_waku();
        if let Some(gateway) = self.gateway.take_shutdown_handle() {
            self.runtime.spawn(async move {
                let _ = gateway.shutdown().await;
            });
        }
        let _ = self.root_shutdown.send(true);
        if !self.wallet_sync_lifecycle_shutdown_started {
            let cleanup = self.wallet_sync_lifecycle.invalidate();
            let _ = cleanup.spawn(&self.runtime);
        }
    }
}

impl WalletRoot {
    fn wallet_db_root_dir(&self) -> Option<PathBuf> {
        self.vault_store
            .as_ref()
            .map(|store| store.db().root_dir().to_path_buf())
    }

    fn save_ui_state(&self) {
        let Some(store) = self.vault_store.as_ref() else {
            return;
        };
        if let Err(error) = save_wallet_ui_state(store.db().as_ref(), &self.ui_state) {
            tracing::warn!(%error, "failed to save wallet UI state");
        }
    }
}

/// The application services and panes `WalletRoot::new` wires in beside the
/// startup result.
pub(in crate::root) struct WalletRootDependencies<'a> {
    pub(in crate::root) options: WalletAppOptions,
    pub(in crate::root) public_transaction_tracker: wallet_ops::PublicTransactionTracker,
    pub(in crate::root) runtime: Handle,
    pub(in crate::root) monitor_state: Shared,
    pub(in crate::root) monitor_event_rx: EventRx,
    pub(in crate::root) monitor: Entity<broadcaster_monitor_gpui::BroadcasterMonitorPane>,
    pub(in crate::root) public_broadcaster_anchor_cache: Arc<TokenAnchorRateCache>,
    pub(in crate::root) public_broadcaster_anchor_refresh: TokenAnchorRefreshHandle,
    pub(in crate::root) logs: Entity<LogsPane>,
    pub(in crate::root) startup_root: &'a gpui::WeakEntity<WalletStartupRoot>,
    pub(in crate::root) maintenance_controller: &'a Entity<WalletMaintenanceController>,
}

impl WalletRoot {
    fn new(
        ready: WalletStartupReady,
        dependencies: WalletRootDependencies<'_>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let WalletStartupReady {
            http,
            waku_config,
            monitor_event_tx,
            vault_store,
            chain_ids,
            initial_chain_id,
            ui_state,
            effective_chain_configs,
            effective_token_registry,
            public_balance_refresh_interval,
            auto_lock_timeout,
            public_broadcaster_policy,
            public_broadcaster_response_timeout,
            public_broadcaster_republish_interval,
            default_allow_suspicious_broadcasters,
            mimic_railway_shields_by_default,
            poi_read_source,
        } = ready;
        let initial_wallet_tab =
            shell::initial_wallet_tab(&effective_chain_configs, initial_chain_id);
        let chain_select_items =
            wallet_header::chain_select_items(&effective_chain_configs, &chain_ids);
        let mut chain_states = BTreeMap::new();
        for chain_id in &chain_ids {
            chain_states.insert(*chain_id, ChainUtxoState::Idle);
        }
        let maintenance_controller = dependencies.maintenance_controller;
        maintenance::attach_active_root(maintenance_controller, cx);
        let reset = maintenance_controller.read(cx).reset();
        let merkle_forest_cache_resetting = reset == maintenance::WalletMaintenanceReset::Merkle;
        let public_sync_cache_resetting = maintenance_blocks_public_sync(reset);
        let gateway =
            gateway::GatewayUi::start(vault_store.as_ref(), &dependencies.runtime, window, cx);
        let (settings_editor, settings_error) =
            Self::initial_settings_editor(&dependencies, &vault_store, window, cx);
        let WalletRootDependencies {
            options,
            public_transaction_tracker,
            runtime,
            monitor_state,
            monitor_event_rx,
            monitor,
            public_broadcaster_anchor_cache,
            public_broadcaster_anchor_refresh,
            logs,
            ..
        } = dependencies;
        let unwrap_unshields_by_default = settings_editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).saved.privacy.unwrap_unshields_by_default);
        let vault_startup = VaultStartup::new(&vault_store);
        let widgets = WalletRootWidgets::new(&logs, mimic_railway_shields_by_default, window, cx);
        let mut platform_attention = platform_attention::PlatformAttentionState::new(window);
        platform_attention.sync_badge_count(0);
        platform_attention.clear_attention();
        let walletconnect = walletconnect::WalletConnectUiState::new(window, cx);
        let tron_settings = Self::initial_tron_settings(settings_editor.as_ref(), cx);
        let chain_select = wallet_header::new_chain_select(
            wallet_header::network_select_items(chain_select_items, tron_settings.enabled),
            SelectedNetwork::Evm(initial_chain_id),
            window,
            cx,
        );
        let wallet_select = wallet_header::new_wallet_select(window, cx);
        let root_weak = cx.weak_entity();
        let root_entity = cx.entity();
        let passphrase_open_ui = cx.new(|cx| PassphraseOpenUi::new(root_entity, window, cx));
        let broadcaster_preference_snapshot =
            install_broadcaster_preference_snapshot(&monitor, root_weak.clone(), cx);
        let utxo_table = utxo::new_utxo_table(&root_weak, &widgets.tx_search_input, window, cx);
        let network_health = http.network_health();
        let tor_bridge_activity = http.tor_bridge_activity_snapshot();
        let sidebar_public_broadcaster_count =
            ethereum_weth_public_broadcaster_count(&monitor_state.read().fee_rows());
        let mut public_broadcaster_sort_seed = [0_u8; 32];
        rand::rng().fill(public_broadcaster_sort_seed.as_mut_slice());
        let anchor_refresh_rx = public_broadcaster_anchor_cache.subscribe_refreshes();
        Self::sync_chain_editor_pricing(
            settings_editor.as_ref(),
            &public_broadcaster_anchor_cache,
            cx,
        );
        // Tron's pricing source is installed now and reads nothing until Tron is selected.
        let tron_public = TronPublicState::new(tron_settings, TronSendState::new(window, cx));
        public_broadcaster_anchor_refresh
            .reconcile_tron(&tron_public.settings, tron_public.route.as_ref());
        let (root_shutdown, _) = watch::channel(false);
        let root = Self {
            selected_network: SelectedNetwork::Evm(initial_chain_id),
            last_evm_chain: initial_chain_id,
            options,
            vault_store: Some(vault_store),
            poi_read_source,
            effective_chain_configs,
            effective_token_registry,
            public_balance_refresh_interval,
            auto_lock: AutoLockState::new(auto_lock_timeout),
            initial_sync_activity: InitialSyncActivity::new(0),
            public_broadcaster_policy,
            public_broadcaster_sort_seed,
            public_broadcaster_response_timeout,
            public_broadcaster_republish_interval,
            default_allow_suspicious_broadcasters,
            mimic_railway_shields_by_default,
            unwrap_unshields_by_default,
            vault_state: vault_startup.state,
            wallet_setup_mode: WalletSetupMode::Choose,
            vault_error: vault_startup.error,
            unlock_in_progress: false,
            repair_cache_error: None,
            setup_password: None,
            vault_view_unlock: None,
            spend_authorization_cache: None,
            spend_authorization_lifetime: SpendAuthorizationLifetime::Once,
            protected_software_seed_session: None,
            pending_software_profile_open: None,
            passphrase_open_ui,
            pending_software_profile_open_operation_generation: 0,
            pending_software_profile_open_lifetime_generation: 0,
            pending_software_profile_base_profile_uuid: None,
            revealed_passphrase_context_id: None,
            view_session: None,
            generated_seed: None,
            generated_seed_revealed: false,
            hardware_wallet_creation: widgets.hardware_wallet_creation,
            #[cfg(feature = "hardware")]
            hardware_profile_unlock: vault::HardwareProfileUnlockState::default(),
            #[cfg(feature = "hardware")]
            hardware_profile: widgets.hardware_profile,
            http,
            rpc_identity_warmup: None,
            active_rpc_chains: BTreeSet::new(),
            network_context_id: format!("{:032x}", rand::random::<u128>()),
            network_status: NetworkStatusUi::new(network_health, tor_bridge_activity, cx),
            root_shutdown,
            gateway,
            prover_cache_build: ProverCacheBuildUi::default(),
            runtime,
            monitor_state,
            monitor_event_tx,
            waku: WakuState::new(waku_config),
            submission_scope: SubmissionNodeScope::new(),
            public_broadcaster_anchor_cache,
            public_broadcaster_anchor_refresh,
            monitor,
            logs_panel: LogsPanelState::new(logs, widgets.logs_search, cx),
            settings_editor,
            maintenance_controller: maintenance_controller.clone(),
            settings_error,
            data_folder_open_error: None,
            active_activity: Activity::Wallet,
            active_wallet_tab: initial_wallet_tab,
            sidebar_manually_collapsed: false,
            sidebar_narrow_expanded: false,
            sidebar_public_broadcaster_count,
            wallet_select,
            wallet_metadata: Vec::new(),
            wallet_options: Vec::new(),
            manage_wallets: ManageWalletsState::default(),
            key_export: KeyExportState::default(),
            update_check: update_check::UpdateCheckUi::default(),
            manage_wallet_label_input: widgets.manage_wallet_label_input,
            selected_wallet_id: None,
            active_wallet_generation: 0,
            wallet_switch_generation: 0,
            wallet_switch_loading_generation: None,
            wallet_switch_delayed: false,
            ui_state,
            chain_select,
            window_handle: window.window_handle(),
            chain_states,
            pending_ppoi_validation_toast: None,
            activity_journal_failures: subscribe_activity_journal_failures(),
            pending_activity_journal_notice: false,
            private_pending_status_dialog_open: false,
            private_asset_presentation_cache: RefCell::default(),
            poi_artifact_cache_progress: BTreeMap::new(),
            poi_artifact_cache_retry_attempts: PoiArtifactCacheRetryAttempts::default(),
            wallet_sync_lifecycle: WalletSyncLifecycle::new(),
            wallet_sync_cleanup_tasks: Vec::new(),
            wallet_sync_lifecycle_shutdown_started: false,
            public_sync_cache_resetting,
            merkle_forest_cache_resetting,
            vault_inputs: widgets.vault_inputs,
            public_accounts: Vec::new(),
            address_book: widgets.address_book,
            governance: GovernanceState::new(
                widgets.governance_participant_picker,
                widgets.governance_proposal_action_amount_input,
                widgets.governance_staking_delegate_input,
                Eip1559GasFeeEditorState::new(window, cx),
            ),
            proposals: ProposalsState::new(initial_chain_id),
            proposal_detail_focus: cx.focus_handle(),
            private_address_book: Vec::new(),
            public_address_book: Vec::new(),
            non_evm_address_book: Vec::new(),
            broadcaster_preferences: BroadcasterPreferencesUi::new(
                broadcaster_preference_snapshot,
                widgets.favorite_broadcaster_input,
                widgets.banned_broadcaster_input,
            ),
            address_book_label_input: widgets.address_book_label_input,
            address_book_save_error: None,
            public_form: widgets.public_form,
            stealth_accounts: None,
            private_swaps: None,
            wallet_activity: widgets.wallet_activity,
            public_balance_cache: PublicBalanceCache::default(),
            tron_public,
            public_transaction_tracker,
            public_transaction_submissions:
                public_transactions::PublicTransactionSubmissions::default(),
            public_transaction_cleanup: None,
            public_balance_snapshot: None,
            public_balance_error: None,
            public_inactive_balance_error: None,
            send_forms: BTreeMap::new(),
            private_action_form: None,
            send_generation_seq: 0,
            unshield_generation_seq: 0,
            cost_estimate_seq: 0,
            unshield_forms: BTreeMap::new(),
            private_broadcaster_progress: None,
            public_broadcaster_task_abort_handles: Vec::new(),
            broadcaster_picker: None,
            unshield_spinner_tick: 0,
            repair_cache_block_input: widgets.repair_cache_block_input,
            tx_search_input: widgets.tx_search_input,
            tx_search_query: Arc::from(""),
            platform_attention_count: 0,
            platform_attention_window_active: window.is_window_active(),
            spend_authorization_attention: Vec::new(),
            platform_attention,
            walletconnect,
            show_spent_utxos: false,
            local_pending_spent_clear_confirming: false,
            blocked_shield_rescue_lookup_generation: 0,
            blocked_shield_rescue_rows: BTreeMap::new(),
            blocked_shield_refunds_in_flight: BTreeSet::new(),
            executor_locked_note_count: 0,
            utxo_table,
            focus_vault_input_on_render: vault_startup.focus_input_on_render,
            device_auth: DeviceAuthUi::new(vault_startup.device_auth),
            add_wallet_dialog_lease: std::rc::Weak::new(),
            add_wallet_device_auth_password: None,
            focus_utxo_table_on_render: false,
            focus_public_account_search_on_render: false,
            wallet_focus: cx.focus_handle(),
        };
        root.subscribe_root_inputs(window, cx);
        Self::start_state_watchers(monitor_event_rx, anchor_refresh_rx, cx);
        root.spawn_background_refreshes(cx);
        root
    }

    /// The settings editor over the saved settings, or the error shown in its
    /// place when they don't load.
    fn initial_settings_editor(
        dependencies: &WalletRootDependencies<'_>,
        store: &Arc<DesktopVaultStore>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> (Option<Entity<WalletSettingsEditor>>, Option<Arc<str>>) {
        let db = store.db();
        match load_wallet_settings(db.as_ref()) {
            Ok(settings) => {
                let store = Arc::clone(store);
                let runtime = dependencies.runtime.clone();
                let startup_root = dependencies.startup_root.clone();
                let maintenance_controller = dependencies.maintenance_controller.clone();
                let active_root = cx.weak_entity();
                let editor = cx.new(|cx| {
                    WalletSettingsEditor::new(
                        store,
                        &dependencies.options.db_path,
                        runtime,
                        settings,
                        maintenance_controller,
                        Some(startup_root),
                        Some(active_root),
                        window,
                        cx,
                    )
                });
                (Some(editor), None)
            }
            Err(error) => (None, Some(Arc::from(error.to_string()))),
        }
    }
}

/// The vault screen to start on, the error to show when the vault can't be
/// inspected, and the device-auth state the first render acts on.
struct VaultStartup {
    state: VaultState,
    error: Option<Arc<str>>,
    focus_input_on_render: bool,
    device_auth: DeviceAuthStartup,
}

impl VaultStartup {
    fn new(store: &DesktopVaultStore) -> Self {
        let (state, error) = match store.vault_exists() {
            Ok(true) => (VaultState::UnlockVault, None),
            Ok(false) => (VaultState::CreateVault, None),
            Err(error) => (
                VaultState::Error(Arc::from("Failed to inspect wallet vault storage")),
                Some(Arc::from(error.to_string())),
            ),
        };
        let focus_input_on_render =
            matches!(state, VaultState::CreateVault | VaultState::UnlockVault);
        let touch_id_supported = wallet_ops::device_auth::device_auth_supported(
            wallet_ops::device_auth::DeviceAuthMethod::TouchId,
        );
        let touch_id_status = store
            .device_auth_status(wallet_ops::device_auth::DeviceAuthMethod::TouchId)
            .unwrap_or(DeviceAuthStatus::Disabled);
        let apple_watch_supported = wallet_ops::device_auth::device_auth_supported(
            wallet_ops::device_auth::DeviceAuthMethod::AppleWatch,
        );
        let apple_watch_status = store
            .device_auth_status(wallet_ops::device_auth::DeviceAuthMethod::AppleWatch)
            .unwrap_or(DeviceAuthStatus::Disabled);
        let touch_id_unlock_on_render = matches!(state, VaultState::UnlockVault)
            && touch_id_supported
            && touch_id_status == DeviceAuthStatus::Enabled;
        Self {
            state,
            error,
            focus_input_on_render,
            device_auth: DeviceAuthStartup {
                touch_id_supported,
                touch_id_status,
                apple_watch_supported,
                apple_watch_status,
                touch_id_unlock_on_render,
            },
        }
    }
}

pub(in crate::root) const fn maintenance_blocks_public_sync(
    reset: maintenance::WalletMaintenanceReset,
) -> bool {
    matches!(
        reset,
        maintenance::WalletMaintenanceReset::Public | maintenance::WalletMaintenanceReset::Poi
    )
}

pub(super) fn new_text_input<T>(
    window: &mut Window,
    cx: &mut Context<'_, T>,
    placeholder: &'static str,
) -> Entity<InputState> {
    new_input(window, cx, placeholder, false)
}

pub(super) fn new_masked_input<T>(
    window: &mut Window,
    cx: &mut Context<'_, T>,
    placeholder: &'static str,
) -> Entity<InputState> {
    new_input(window, cx, placeholder, true)
}

fn new_input<T>(
    window: &mut Window,
    cx: &mut Context<'_, T>,
    placeholder: &'static str,
    masked: bool,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(masked)
    })
}

pub(super) fn new_prefilled_input<T>(
    window: &mut Window,
    cx: &mut Context<'_, T>,
    placeholder: &'static str,
    value: impl Into<SharedString>,
) -> Entity<InputState> {
    let value = value.into();
    cx.new(move |cx| {
        let mut input = InputState::new(window, cx).placeholder(placeholder);
        input.set_value(value.clone(), window, cx);
        input
    })
}

fn format_report_chain(error: &eyre::Report) -> String {
    let mut parts = error.chain().map(ToString::to_string);
    let Some(mut message) = parts.next() else {
        return error.to_string();
    };
    for part in parts {
        if message.ends_with(&part) {
            continue;
        }
        message.push_str(": ");
        message.push_str(&part);
    }
    message
}
