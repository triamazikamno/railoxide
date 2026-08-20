use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use broadcaster_monitor::{EventRx, EventTx, Shared, publish_revision};
use broadcaster_monitor_waku::{RelayNetworkMode, WakuMonitorConfig, spawn_workers_until_shutdown};
#[cfg(feature = "hardware")]
use gpui::FocusHandle;
use gpui::{AppContext, Context, Entity, Focusable, Pixels, SharedString, Window, px};
use gpui_component::{
    IndexPath, WindowExt,
    input::{InputEvent, InputState},
    resizable::ResizableState,
    select::{SearchableVec, SelectEvent, SelectState},
    table::{TableEvent, TableState},
};
use rand::RngExt;
use tokio::runtime::Handle;
use tokio::sync::watch;
use ui::logs::LogsPane;
use ui::theme::APP_TEXT_SIZE;
use wallet_ops::{
    BlockedShieldRescueUtxoId, BroadcasterFeePolicy, HttpContext, PoiArtifactCacheProgress,
    PoiReadSource, ProverCacheBuildProgress, PublicBalanceSnapshot, SponsoredSelfBroadcastCommand,
    TokenAnchorRateCache, TokenAnchorRefreshHandle, WakuDeliveryClient, WalletNetworkHealth,
    hardware::HardwareWalletSyncIntent,
    settings::{
        EffectiveChainConfig, EffectiveTokenRegistry, WalletUiState, load_wallet_settings,
        save_wallet_ui_state,
    },
    subscribe_prover_cache_build,
    vault::{
        BroadcasterPreferences, DesktopVaultStore, DesktopViewSession, GeneratedSeedMaterial,
        PrivateAddressBookEntry, ProtectedSoftwareSeedSession, PublicAccountMetadata,
        PublicAddressBookEntry, ViewUnlock, WalletMetadataBundle,
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
mod dialogs;
mod gas_fee;
mod key_export;
mod maintenance;
mod manage_wallets;
mod network;
mod platform_attention;
mod private_action;
mod private_assets;
mod private_broadcaster;
mod public_account;
mod public_action;
mod public_balances;
mod public_broadcaster;
mod public_broadcaster_cost;
mod retry;
mod settings;
mod shell;
mod sidebar;
mod spend_authorization;
mod startup;
mod tokens;
mod ui_helpers;
mod utxo;
mod vault;
mod vault_ui;
mod wallet_header;
mod walletconnect;

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
use broadcaster_preferences::{
    broadcaster_preference_is_banned, broadcaster_preference_is_favorite,
};
use broadcaster_view::{BroadcasterActivityTab, BroadcasterPreferenceListKind};
use chain_load::{
    ChainUtxoState, WalletSyncLifecycle, WalletSyncLifecycleCleanupTask, chain_load_overrides,
};
use gas_fee::Eip1559GasFeeEditorState;
use key_export::KeyExportState;
use maintenance::WalletMaintenanceController;
use manage_wallets::ManageWalletsState;
use network::TorExitIpQueryState;
use private_action::{
    DeliveryFormKind, DeliveryMode, PrivateActionFormState, SendFormState, UnshieldAsset,
    UnshieldAssetKey, UnshieldFormState,
};
use private_broadcaster::PrivateBroadcasterProgressState;
use public_account::{HardwarePublicAccountDerivationStatus, PublicAccountFormState};
use public_action::{PublicActionMode, PublicSendKind};
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
};
use settings::WalletSettingsEditor;
use shell::{PoiArtifactCacheRetryAttempts, WalletTab};
#[cfg(test)]
use shell::{ppoi_hover_detail, ppoi_hover_heading};
use sidebar::Activity;
use spend_authorization::{SpendAuthorizationCache, SpendAuthorizationLifetime};
use startup::WalletStartupRoot;
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
    scrollable_dialog_content, secondary_dialog_content_width, token_label_row,
};
use utxo::{
    BlockedShieldRescueRowState, UtxoDelegate, should_focus_utxo_table, should_refresh_utxo_ages,
};
use vault::{
    PassphraseOpenUi, PendingSoftwareProfileOpen, VaultState, WalletOption, WalletSetupMode,
    vault_error_kind,
};
use wallet_header::{ChainSelectItem, WalletSelectItem};

#[cfg(test)]
use broadcaster_picker::{
    BroadcasterChoice, BroadcasterPickerEntry, BroadcasterPickerFeeEstimateRetryState,
    BroadcasterPickerFeeStatus, BroadcasterPickerGroupKey, BroadcasterPickerRow,
    BroadcasterPickerTier, BroadcasterPickerViewMode,
    broadcaster_candidate_estimated_fee_amount_for_estimate,
    broadcaster_choice_supported_by_candidates, broadcaster_picker_fee_status,
    broadcaster_picker_fee_status_detail, broadcaster_picker_fee_text_colors,
    broadcaster_picker_scroll_hint_visible, group_minimum_estimated_fee_labels,
    project_broadcaster_picker_entries, should_preserve_estimate_after_broadcaster_policy_change,
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
    PrivateActionMetric, PrivateWalletRecipientSource, RecipientOption, RecipientOptionSource,
    SEND_AUTHORIZATION_FAILED_ERROR, SelfBroadcastNativeBalanceState,
    UNSHIELD_AUTHORIZATION_FAILED_ERROR, adjusted_amount_for_max_change,
    can_save_private_recipient, can_save_public_recipient, default_self_broadcast_gas_payer_uuid,
    form_error_clears_public_broadcaster_cost_estimate, format_exact_asset_amount_for_display,
    format_form_error_for_asset, normalized_address_book_save_label,
    private_action_assets_from_snapshot, private_action_metric_display_amount,
    private_action_metrics, private_send_recipient_options, private_unshield_recipient_options,
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
    public_action_progress_steps, public_action_step_color, public_action_step_detail,
    public_action_step_is_final_handoff, public_action_step_uses_stop_marker,
    public_action_uses_railway_authorization_ceiling,
};
#[cfg(test)]
use public_balances::{
    merge_public_balance_snapshot, public_asset_icon_path, public_balance_usd_label,
};
#[cfg(test)]
use public_broadcaster::{
    fee_token_option_has_eligible_broadcaster, public_broadcaster_fee_token_options_from_snapshot,
    required_relay_adapt_for_unshield, resolve_selected_public_broadcaster_fee_token,
};
#[cfg(test)]
use public_broadcaster_cost::{
    CostEstimateStatus, PublicBroadcasterCostDisplay, format_public_broadcaster_fee_margin,
    public_broadcaster_cost_status_text, should_render_public_broadcaster_cost_preview,
};
#[cfg(test)]
use settings::{
    PriceAnchorComponentDialogValues, PriceAnchorDialogValues, SettingsApplyMode,
    StartupSettingsActionState, add_chain_rpc_endpoint, add_poi_gateway_url, add_waku_direct_peer,
    add_waku_dns_enr_tree, add_waku_doh_fallback_endpoint, auto_lock_timeout_from_value,
    auto_lock_timeout_options, auto_lock_timeout_value, classify_settings_apply_mode,
    display_chain_contract_settings, display_chain_quick_sync_endpoint,
    display_chain_rpc_endpoints, display_price_anchor_entries, display_sponsored_bundle_relays,
    display_token_entries, display_waku_direct_peers, display_waku_dns_enr_trees,
    display_waku_doh_endpoint, display_waku_doh_fallback_endpoints, format_anchor_bps_exact_range,
    format_anchor_bps_percent, format_anchor_bps_percent_range, format_anchor_premium_range,
    price_anchor_dialog_values_from_entry, price_anchor_override_from_dialog_values,
    price_anchor_token_primary_label, remove_chain_rpc_endpoint, remove_poi_gateway_url,
    remove_sponsored_bundle_relay, remove_waku_direct_peer, remove_waku_dns_enr_tree,
    remove_waku_doh_fallback_endpoint, set_chain_rpc_endpoint, set_poi_gateway_url,
    set_price_anchor_override, set_sponsored_bundle_relay, set_waku_direct_peer,
    set_waku_dns_enr_tree, set_waku_doh_fallback_endpoint, settings_draft_after_discard,
    settings_restart_action_enabled, settings_restart_reuses_active_network,
    settings_save_action_enabled, should_show_proxy_url_setting, should_show_proxy_waku_disclaimer,
    startup_settings_action_state,
};
#[cfg(test)]
use sidebar::sidebar_primary_activity_order;
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
    recoverable_poi_candidate_count, shield_poi_wait_display,
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
use wallet_header::{parse_repair_cache_block, repair_cache_help_text};
#[cfg(test)]
use wallet_ops::public_broadcaster_candidates_for_asset;
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
const PROVER_CACHE_BUILD_DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);

pub(super) const fn should_apply_background_focus(has_active_dialog: bool) -> bool {
    !has_active_dialog
}

pub(crate) struct WalletRoot {
    options: WalletAppOptions,
    vault_store: Option<Arc<DesktopVaultStore>>,
    poi_read_source: PoiReadSource,
    effective_chain_configs: BTreeMap<u64, EffectiveChainConfig>,
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
    hardware_wallet_creation_in_progress: bool,
    hardware_wallet_creation_generation: u64,
    hardware_wallet_creation_intent: Option<HardwareWalletSyncIntent>,
    hardware_wallet_restore_account_index_input: Entity<InputState>,
    hardware_wallet_restore_account_index_set: bool,
    #[cfg(feature = "hardware")]
    hardware_profile_unlock: vault::HardwareProfileUnlockState,
    #[cfg(feature = "hardware")]
    active_hardware_profile: Option<wallet_ops::vault::HardwareProfileMetadata>,
    #[cfg(feature = "hardware")]
    hardware_profile_password_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    hardware_profile_label_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    hardware_profile_recovery_start_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    hardware_profile_recovery_count_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    hardware_profile_exact_index_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    trezor_app_passphrase_input: Entity<InputState>,
    #[cfg(feature = "hardware")]
    trezor_passphrase_mode_focus: FocusHandle,
    http: HttpContext,
    network_health: WalletNetworkHealth,
    tor_bridge_activity: Option<wallet_ops::TorBridgeActivitySnapshot>,
    tor_download_rate: Option<u64>,
    root_shutdown: watch::Sender<bool>,
    network_status_popover_open: bool,
    network_status_error: Option<Arc<str>>,
    tor_exit_ip_query: TorExitIpQueryState,
    tor_exit_ip_query_generation: u64,
    tor_state_reset_confirming: bool,
    prover_cache_build_progress: Option<ProverCacheBuildProgress>,
    prover_cache_build_popover_open: bool,
    prover_cache_build_monitor_active: bool,
    prover_cache_build_completed: bool,
    runtime: Handle,
    monitor_state: Shared,
    monitor_event_tx: EventTx,
    waku_config: WakuMonitorConfig,
    waku_runtime: Option<WalletWakuRuntime>,
    waku_session_generation: u64,
    waku_stopping_generation: Option<WakuStoppingState>,
    public_broadcaster_anchor_cache: Arc<TokenAnchorRateCache>,
    public_broadcaster_anchor_refresh: TokenAnchorRefreshHandle,
    monitor: Entity<broadcaster_monitor_gpui::BroadcasterMonitorPane>,
    logs: Entity<LogsPane>,
    settings_editor: Option<Entity<WalletSettingsEditor>>,
    maintenance_controller: Entity<WalletMaintenanceController>,
    settings_error: Option<Arc<str>>,
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
    manage_wallet_label_input: Entity<InputState>,
    selected_wallet_id: Option<Arc<str>>,
    active_wallet_generation: u64,
    wallet_switch_generation: u64,
    wallet_switch_delayed: bool,
    selected_chain: u64,
    ui_state: WalletUiState,
    chain_select: Entity<SelectState<Vec<ChainSelectItem>>>,
    chain_states: BTreeMap<u64, ChainUtxoState>,
    pending_ppoi_validation_toast: Option<(Arc<str>, u64)>,
    private_pending_status_dialog_open: bool,
    poi_artifact_cache_progress: BTreeMap<u64, PoiArtifactCacheProgress>,
    poi_artifact_cache_retry_attempts: PoiArtifactCacheRetryAttempts,
    wallet_sync_lifecycle: WalletSyncLifecycle,
    wallet_sync_cleanup_tasks: Vec<WalletSyncLifecycleCleanupTask>,
    wallet_sync_lifecycle_shutdown_started: bool,
    public_sync_cache_resetting: bool,
    merkle_forest_cache_resetting: bool,
    unlock_password_input: Entity<InputState>,
    new_password_input: Entity<InputState>,
    confirm_password_input: Entity<InputState>,
    wallet_name_input: Entity<InputState>,
    add_wallet_password_input: Entity<InputState>,
    import_mnemonic_input: Entity<InputState>,
    public_accounts: Vec<PublicAccountMetadata>,
    address_book: AddressBookState,
    private_address_book: Vec<PrivateAddressBookEntry>,
    public_address_book: Vec<PublicAddressBookEntry>,
    broadcaster_preferences: BroadcasterPreferences,
    broadcaster_preference_snapshot: Arc<RwLock<BroadcasterPreferences>>,
    broadcaster_preference_error: Option<Arc<str>>,
    active_broadcaster_tab: BroadcasterActivityTab,
    favorite_broadcaster_input: Entity<InputState>,
    banned_broadcaster_input: Entity<InputState>,
    address_book_label_input: Entity<InputState>,
    address_book_save_error: Option<Arc<str>>,
    public_form: PublicAccountFormState,
    public_balance_snapshot: Option<Arc<PublicBalanceSnapshot>>,
    public_balance_error: Option<Arc<str>>,
    public_balance_refreshing: bool,
    public_balance_generation: u64,
    public_inactive_balance_error: Option<Arc<str>>,
    public_inactive_balance_refreshing: bool,
    public_inactive_balance_generation: u64,
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
    walletconnect_attention_count: usize,
    walletconnect_window_active: bool,
    platform_attention: platform_attention::PlatformAttentionState,
    walletconnect: walletconnect::WalletConnectUiState,
    show_spent_utxos: bool,
    local_pending_spent_clear_confirming: bool,
    blocked_shield_rescue_lookup_generation: u64,
    blocked_shield_rescue_rows: BTreeMap<BlockedShieldRescueUtxoId, BlockedShieldRescueRowState>,
    blocked_shield_refunds_in_flight: BTreeSet<BlockedShieldRescueUtxoId>,
    utxo_table: Entity<TableState<UtxoDelegate>>,
    focus_vault_input_on_render: bool,
    focus_utxo_table_on_render: bool,
    focus_public_account_search_on_render: bool,
    logs_open: bool,
    drawer_split: Entity<ResizableState>,
}

struct WalletWakuRuntime {
    client: Arc<WakuDeliveryClient>,
    worker_shutdown: watch::Sender<bool>,
    completion: WakuWorkerCompletionToken,
    generation: u64,
}

#[derive(Clone)]
pub(super) struct WakuWorkerCompletionToken {
    quiesced_rx: watch::Receiver<bool>,
}

impl WakuWorkerCompletionToken {
    async fn wait(mut self) -> Result<(), String> {
        loop {
            if *self.quiesced_rx.borrow() {
                return Ok(());
            }
            self.quiesced_rx
                .changed()
                .await
                .map_err(|_| "Waku worker ended before quiescence was established".to_string())?;
        }
    }

    #[cfg(test)]
    fn closed_for_test() -> Self {
        let (_quiesced_tx, quiesced_rx) = watch::channel(false);
        Self { quiesced_rx }
    }

    #[cfg(test)]
    fn channel_for_test() -> (Self, watch::Sender<bool>) {
        let (quiesced_tx, quiesced_rx) = watch::channel(false);
        (Self { quiesced_rx }, quiesced_tx)
    }
}

struct WakuWorkerQuiescenceGuard {
    quiesced_tx: watch::Sender<bool>,
}

impl WakuWorkerQuiescenceGuard {
    const fn new(quiesced_tx: watch::Sender<bool>) -> Self {
        Self { quiesced_tx }
    }
}

impl Drop for WakuWorkerQuiescenceGuard {
    fn drop(&mut self) {
        let _ = self.quiesced_tx.send(true);
    }
}

async fn run_waku_worker_task(
    worker: impl Future<Output = eyre::Result<()>>,
    quiescence_guard: WakuWorkerQuiescenceGuard,
) -> eyre::Result<()> {
    let result = worker.await;
    drop(quiescence_guard);
    result
}

struct WakuStoppingState {
    generation: u64,
    completion: WakuWorkerCompletionToken,
}

#[cfg(test)]
impl WakuStoppingState {
    fn for_test(generation: u64) -> Self {
        Self {
            generation,
            completion: WakuWorkerCompletionToken::closed_for_test(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WakuWorkerCompletionKind {
    Clean,
    WorkerError,
    TaskError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WakuWorkerCompletionAction {
    FinalizedStop { restart: bool },
    HandleActiveFailure,
    Ignore,
}

fn should_start_waku_runtime(
    view_unlocked: bool,
    runtime_active: bool,
    runtime_stopping: bool,
    lifecycle_shutdown_started: bool,
    network_mode: RelayNetworkMode,
) -> bool {
    view_unlocked
        && !runtime_active
        && !runtime_stopping
        && !lifecycle_shutdown_started
        && network_mode != RelayNetworkMode::Proxy
}

fn should_start_waku_for_delivery(
    delivery_mode: DeliveryMode,
    view_unlocked: bool,
    runtime_active: bool,
    runtime_stopping: bool,
    lifecycle_shutdown_started: bool,
    network_mode: RelayNetworkMode,
) -> bool {
    delivery_mode == DeliveryMode::PublicBroadcaster
        && should_start_waku_runtime(
            view_unlocked,
            runtime_active,
            runtime_stopping,
            lifecycle_shutdown_started,
            network_mode,
        )
}

fn build_waku_client_if_needed<C, E>(
    should_start: bool,
    build: impl FnOnce() -> Result<C, E>,
) -> Result<Option<C>, E> {
    if should_start {
        build().map(Some)
    } else {
        Ok(None)
    }
}

fn active_waku_client(runtime: Option<&WalletWakuRuntime>) -> Option<Arc<WakuDeliveryClient>> {
    runtime.map(|runtime| Arc::clone(&runtime.client))
}

fn refresh_active_waku(runtime: Option<&WalletWakuRuntime>) -> bool {
    runtime.is_some_and(|runtime| runtime.client.refresh_network_session())
}

fn take_waku_runtime_for_stop(
    runtime: &mut Option<WalletWakuRuntime>,
) -> Option<WakuStoppingState> {
    let runtime = runtime.take()?;
    let _ = runtime.worker_shutdown.send(true);
    Some(WakuStoppingState {
        generation: runtime.generation,
        completion: runtime.completion,
    })
}

#[cfg(test)]
fn stop_waku_runtime(
    runtime: &mut Option<WalletWakuRuntime>,
    monitor_state: &Shared,
    monitor_event_tx: &EventTx,
) -> bool {
    let stopped = take_waku_runtime_for_stop(runtime).is_some();
    if stopped && let Some(rev) = monitor_state.write().clear() {
        publish_revision(monitor_event_tx, rev);
    }
    stopped
}

fn complete_waku_worker_generation(
    active_generation: Option<u64>,
    stopping_generation: &mut Option<WakuStoppingState>,
    generation: u64,
    completion: WakuWorkerCompletionKind,
    view_unlocked: bool,
    lifecycle_shutdown_started: bool,
    monitor_state: &Shared,
    monitor_event_tx: &EventTx,
) -> WakuWorkerCompletionAction {
    if stopping_generation
        .as_ref()
        .is_some_and(|stopping| stopping.generation == generation)
    {
        *stopping_generation = None;
        if !lifecycle_shutdown_started && let Some(rev) = monitor_state.write().clear() {
            publish_revision(monitor_event_tx, rev);
        }
        return WakuWorkerCompletionAction::FinalizedStop {
            restart: view_unlocked && !lifecycle_shutdown_started,
        };
    }

    if active_generation == Some(generation) && completion != WakuWorkerCompletionKind::Clean {
        WakuWorkerCompletionAction::HandleActiveFailure
    } else {
        WakuWorkerCompletionAction::Ignore
    }
}

impl Drop for WalletRoot {
    fn drop(&mut self) {
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
        let _ = self.root_shutdown.send(true);
        if !self.wallet_sync_lifecycle_shutdown_started {
            let cleanup = self.wallet_sync_lifecycle.invalidate();
            let _ = cleanup.spawn(&self.runtime);
        }
    }
}

impl WalletRoot {
    pub(super) fn active_waku(&self) -> Option<Arc<WakuDeliveryClient>> {
        active_waku_client(self.waku_runtime.as_ref())
    }

    pub(in crate::root) fn ensure_waku_for_delivery(
        &mut self,
        delivery_mode: DeliveryMode,
        cx: &mut Context<'_, Self>,
    ) {
        if should_start_waku_for_delivery(
            delivery_mode,
            self.vault_view_unlock.is_some(),
            self.waku_runtime.is_some(),
            self.waku_stopping_generation.is_some(),
            self.wallet_sync_lifecycle_shutdown_started,
            self.waku_config.network.mode,
        ) {
            self.ensure_waku_started(cx);
        }
    }

    pub(super) fn ensure_waku_started(&mut self, cx: &mut Context<'_, Self>) {
        let should_start = should_start_waku_runtime(
            self.vault_view_unlock.is_some(),
            self.waku_runtime.is_some(),
            self.waku_stopping_generation.is_some(),
            self.wallet_sync_lifecycle_shutdown_started,
            self.waku_config.network.mode,
        );
        let client =
            match build_waku_client_if_needed(should_start, || self.waku_config.build_client()) {
                Ok(Some(client)) => client,
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(%error, "wallet Waku delivery runtime failed to start");
                    self.network_status_error = Some(Arc::from(format!(
                        "Waku delivery network is unavailable: {}",
                        format_report_chain(&error)
                    )));
                    cx.notify();
                    return;
                }
            };
        self.waku_session_generation = self.waku_session_generation.wrapping_add(1);
        let generation = self.waku_session_generation;
        let (worker_shutdown, shutdown_rx) = watch::channel(false);
        let (quiesced_tx, quiesced_rx) = watch::channel(false);
        let completion = WakuWorkerCompletionToken { quiesced_rx };
        let config = self.waku_config.clone();
        let worker_client = Arc::clone(&client);
        let worker_monitor_state = self.monitor_state.clone();
        let worker_event_tx = self.monitor_event_tx.clone();
        let quiescence_guard = WakuWorkerQuiescenceGuard::new(quiesced_tx);
        let worker = self.runtime.spawn(async move {
            run_waku_worker_task(
                spawn_workers_until_shutdown(
                    config,
                    worker_client,
                    worker_monitor_state,
                    worker_event_tx,
                    shutdown_rx,
                ),
                quiescence_guard,
            )
            .await
        });
        self.waku_runtime = Some(WalletWakuRuntime {
            client,
            worker_shutdown,
            completion,
            generation,
        });
        self.network_status_error = None;

        cx.spawn(async move |this, cx| {
            let result = worker.await;
            let completion = match &result {
                Ok(Ok(())) => WakuWorkerCompletionKind::Clean,
                Ok(Err(_)) => WakuWorkerCompletionKind::WorkerError,
                Err(_) => WakuWorkerCompletionKind::TaskError,
            };
            let _ = this.update(cx, |root, cx| {
                let action = complete_waku_worker_generation(
                    root.waku_runtime.as_ref().map(|runtime| runtime.generation),
                    &mut root.waku_stopping_generation,
                    generation,
                    completion,
                    root.vault_view_unlock.is_some(),
                    root.wallet_sync_lifecycle_shutdown_started,
                    &root.monitor_state,
                    &root.monitor_event_tx,
                );
                match (action, result) {
                    (
                        WakuWorkerCompletionAction::FinalizedStop { restart },
                        stopping_result,
                    ) => {
                        match stopping_result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                tracing::debug!(%error, "Waku delivery worker failed while stopping");
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Waku delivery worker task failed while stopping");
                            }
                        }
                        if restart {
                            root.ensure_waku_started(cx);
                        } else {
                            cx.notify();
                        }
                    }
                    (WakuWorkerCompletionAction::HandleActiveFailure, Ok(Err(error))) => {
                        root.stop_waku();
                        root.waku_stopping_generation = None;
                        tracing::error!(%error, "wallet Waku delivery monitor workers failed to start");
                        root.network_status_error = Some(Arc::from(format!(
                            "Waku delivery network is unavailable: {}",
                            format_report_chain(&error)
                        )));
                        cx.notify();
                    }
                    (WakuWorkerCompletionAction::HandleActiveFailure, Err(error)) => {
                        root.stop_waku();
                        root.waku_stopping_generation = None;
                        tracing::error!(%error, "wallet Waku delivery monitor task failed");
                        root.network_status_error = Some(Arc::from(format!(
                            "Waku delivery network monitor failed: {error}"
                        )));
                        cx.notify();
                    }
                    (
                        WakuWorkerCompletionAction::HandleActiveFailure,
                        Ok(Ok(())),
                    )
                    | (WakuWorkerCompletionAction::Ignore, _) => {}
                }
            });
        })
        .detach();
    }

    pub(super) fn stop_waku(&mut self) -> bool {
        let stopping = take_waku_runtime_for_stop(&mut self.waku_runtime);
        let stopped = stopping.is_some();
        if stopped {
            self.waku_stopping_generation = stopping;
            if !self.wallet_sync_lifecycle_shutdown_started
                && let Some(rev) = self.monitor_state.write().clear()
            {
                publish_revision(&self.monitor_event_tx, rev);
            }
        }
        stopped
    }

    pub(super) fn stop_waku_for_root_replacement(&mut self) -> Option<WakuWorkerCompletionToken> {
        if let Some(stopping) = take_waku_runtime_for_stop(&mut self.waku_runtime) {
            self.waku_stopping_generation = Some(stopping);
        }
        self.waku_stopping_generation
            .as_ref()
            .map(|stopping| stopping.completion.clone())
    }

    const fn is_prover_cache_building(&self) -> bool {
        self.prover_cache_build_progress.is_some()
    }

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

    fn ensure_prover_cache_build_monitor(&mut self, cx: &Context<'_, Self>) {
        if self.prover_cache_build_monitor_active {
            return;
        }
        let Some(db_path) = self.wallet_db_root_dir() else {
            return;
        };
        self.prover_cache_build_monitor_active = true;
        cx.spawn(async move |this, cx| {
            loop {
                if let Some(mut progress_rx) = subscribe_prover_cache_build(&db_path) {
                    let progress = progress_rx.borrow().clone();
                    if this
                        .update(cx, |root, cx| {
                            root.set_prover_cache_build_progress(progress, cx);
                        })
                        .is_err()
                    {
                        break;
                    }

                    loop {
                        if progress_rx.changed().await.is_err() {
                            let _ = this.update(cx, |root, cx| {
                                root.set_prover_cache_build_progress(None, cx);
                            });
                            break;
                        }
                        let progress = progress_rx.borrow().clone();
                        let is_complete = progress.is_none();
                        if this
                            .update(cx, |root, cx| {
                                root.set_prover_cache_build_progress(progress, cx);
                            })
                            .is_err()
                        {
                            return;
                        }
                        if is_complete {
                            break;
                        }
                    }
                }

                cx.background_executor()
                    .timer(PROVER_CACHE_BUILD_DISCOVERY_INTERVAL)
                    .await;
            }
        })
        .detach();
    }

    fn set_prover_cache_build_progress(
        &mut self,
        progress: Option<ProverCacheBuildProgress>,
        cx: &mut Context<'_, Self>,
    ) {
        self.prover_cache_build_progress = progress;
        if self.prover_cache_build_progress.is_none() {
            self.prover_cache_build_popover_open = false;
        }
        cx.notify();
    }

    fn update_prover_cache_build_progress(
        &mut self,
        progress: ProverCacheBuildProgress,
        cx: &mut Context<'_, Self>,
    ) {
        self.prover_cache_build_progress = Some(progress);
        cx.notify();
    }

    fn finish_prover_cache_build_progress(&mut self, cx: &mut Context<'_, Self>) {
        self.prover_cache_build_progress = None;
        self.prover_cache_build_popover_open = false;
        cx.notify();
    }

    fn set_prover_cache_build_popover_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if self.prover_cache_build_popover_open != open {
            self.prover_cache_build_popover_open = open;
            cx.notify();
        }
    }
}

impl WalletRoot {
    fn new(
        options: WalletAppOptions,
        http: HttpContext,
        vault_store: Arc<DesktopVaultStore>,
        chain_ids: &[u64],
        initial_chain_id: u64,
        ui_state: WalletUiState,
        effective_chain_configs: BTreeMap<u64, EffectiveChainConfig>,
        effective_token_registry: EffectiveTokenRegistry,
        public_balance_refresh_interval: Duration,
        auto_lock_timeout: Option<Duration>,
        public_broadcaster_policy: BroadcasterFeePolicy,
        public_broadcaster_response_timeout: Duration,
        public_broadcaster_republish_interval: Duration,
        default_allow_suspicious_broadcasters: bool,
        mimic_railway_shields_by_default: bool,
        poi_read_source: PoiReadSource,
        runtime: Handle,
        monitor_state: Shared,
        waku_config: WakuMonitorConfig,
        monitor_event_tx: EventTx,
        public_broadcaster_anchor_cache: Arc<TokenAnchorRateCache>,
        public_broadcaster_anchor_refresh: TokenAnchorRefreshHandle,
        mut monitor_event_rx: EventRx,
        monitor: Entity<broadcaster_monitor_gpui::BroadcasterMonitorPane>,
        logs: Entity<LogsPane>,
        startup_root: &gpui::WeakEntity<WalletStartupRoot>,
        maintenance_controller: &Entity<WalletMaintenanceController>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let chain_select_items: Vec<_> = chain_ids
            .iter()
            .copied()
            .map(|chain_id| ChainSelectItem { chain_id })
            .collect();
        let selected_chain_index = chain_ids
            .iter()
            .position(|chain_id| *chain_id == initial_chain_id)
            .map(|index| IndexPath::default().row(index));
        let mut chain_states = BTreeMap::new();
        for chain_id in chain_ids {
            chain_states.insert(*chain_id, ChainUtxoState::Idle);
        }
        let active_root = cx.weak_entity();
        maintenance_controller.update(cx, |controller, _cx| {
            controller.set_active_root(active_root.clone());
        });
        let merkle_forest_cache_resetting =
            maintenance_controller.read(cx).reset() == maintenance::WalletMaintenanceReset::Merkle;
        let public_sync_cache_resetting =
            maintenance_blocks_public_sync(maintenance_controller.read(cx).reset());
        let vault_store = Some(vault_store);
        let (settings_editor, settings_error) = match vault_store.as_ref() {
            Some(store) => {
                let db = store.db();
                match load_wallet_settings(db.as_ref()) {
                    Ok(settings) => (
                        Some(cx.new({
                            let store = Arc::clone(store);
                            let runtime = runtime.clone();
                            let startup_root = startup_root.clone();
                            let maintenance_controller = maintenance_controller.clone();
                            let active_root = active_root.clone();
                            move |cx| {
                                WalletSettingsEditor::new(
                                    store,
                                    runtime,
                                    settings,
                                    maintenance_controller,
                                    Some(startup_root),
                                    Some(active_root),
                                    cx,
                                )
                            }
                        })),
                        None,
                    ),
                    Err(error) => (None, Some(Arc::from(error.to_string()))),
                }
            }
            None => (None, Some(Arc::from("Wallet database is unavailable"))),
        };
        let (vault_state, vault_error) = match vault_store.as_ref() {
            Some(store) => match store.vault_exists() {
                Ok(true) => (VaultState::UnlockVault, None),
                Ok(false) => (VaultState::CreateVault, None),
                Err(error) => (
                    VaultState::Error(Arc::from("Failed to inspect wallet vault storage")),
                    Some(Arc::from(error.to_string())),
                ),
            },
            None => (
                VaultState::Error(Arc::from("Failed to open wallet vault storage")),
                None,
            ),
        };
        let focus_vault_input_on_render = matches!(
            vault_state,
            VaultState::CreateVault | VaultState::UnlockVault
        );
        let unlock_password_input = new_masked_input(window, cx, "vault password");
        let new_password_input = new_masked_input(window, cx, "new vault password");
        let confirm_password_input = new_masked_input(window, cx, "confirm vault password");
        let wallet_name_input = new_text_input(window, cx, "wallet name");
        let manage_wallet_label_input = new_text_input(window, cx, "wallet label");
        let add_wallet_password_input = new_masked_input(window, cx, "vault password");
        let hardware_wallet_restore_account_index_input =
            new_text_input(window, cx, "optional restore account index");
        #[cfg(feature = "hardware")]
        let hardware_profile_password_input = new_masked_input(window, cx, "vault password");
        #[cfg(feature = "hardware")]
        let hardware_profile_label_input = new_text_input(window, cx, "hardware profile label");
        #[cfg(feature = "hardware")]
        let hardware_profile_recovery_start_input = new_text_input(window, cx, "start index");
        #[cfg(feature = "hardware")]
        let hardware_profile_recovery_count_input = new_text_input(window, cx, "count");
        #[cfg(feature = "hardware")]
        let hardware_profile_exact_index_input = new_text_input(window, cx, "account index");
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase_input = new_masked_input(window, cx, "Trezor passphrase");
        #[cfg(feature = "hardware")]
        let trezor_passphrase_mode_focus = cx.focus_handle();
        let import_mnemonic_input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 6)
                .placeholder("paste recovery phrase")
        });
        let public_account_search_input = new_text_input(window, cx, "search accounts");
        let address_book_search_input = new_text_input(window, cx, "search saved recipients");
        let favorite_broadcaster_input =
            new_text_input(window, cx, "favorite broadcaster 0zk address");
        let banned_broadcaster_input = new_text_input(window, cx, "banned broadcaster 0zk address");
        let address_book_label_input = new_text_input(window, cx, "recipient label");
        let address_book = AddressBookState {
            search_input: address_book_search_input,
            add_label_input: new_text_input(window, cx, "recipient label"),
            add_address_input: new_text_input(window, cx, "0zk or 0x recipient"),
            edit_label_input: new_text_input(window, cx, "recipient label"),
            edit_address_input: new_text_input(window, cx, "recipient address"),
            search_query: Arc::from(""),
            editing_entry: None,
            pending_delete: None,
            error: None,
        };
        let public_form = PublicAccountFormState {
            add_label_input: new_text_input(window, cx, "account label"),
            add_password_input: new_masked_input(window, cx, "vault password"),
            import_label_input: new_text_input(window, cx, "account label"),
            import_private_key_input: new_masked_input(window, cx, "private key hex"),
            import_password_input: new_masked_input(window, cx, "vault password"),
            edit_label_input: new_text_input(window, cx, "account label"),
            search_input: public_account_search_input.clone(),
            send_recipient_input: new_text_input(window, cx, "0x recipient"),
            send_amount_input: new_text_input(window, cx, "amount"),
            advanced_send_to_input: new_text_input(window, cx, "0x…"),
            advanced_send_value_input: new_text_input(window, cx, "0"),
            advanced_send_data_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .auto_grow(4, 12)
                    .placeholder("0x…")
            }),
            shield_amount_input: new_text_input(window, cx, "amount"),
            send_gas_fee: Eip1559GasFeeEditorState::new(window, cx),
            shield_gas_fee: Eip1559GasFeeEditorState::new(window, cx),
            shield_gas_fee_authorization_ceiling: None,
            import_global: false,
            selected_account_uuid: None,
            editing_account_uuid: None,
            search_query: Arc::from(""),
            selected_asset: None,
            mimic_railway_shield: mimic_railway_shields_by_default,
            action_mode: PublicActionMode::Shield,
            public_send_kind: PublicSendKind::Transfer,
            advanced_send_estimate: None,
            advanced_send_estimate_invalidated: false,
            advanced_send_estimate_pending: false,
            advanced_send_estimate_generation: 0,
            advanced_send_to_error: None,
            advanced_send_value_error: None,
            advanced_send_data_error: None,
            action_generation: 0,
            action_progress: Vec::new(),
            action_fee_authorization_review: None,
            expanded_action_error_steps: BTreeSet::new(),
            action_progress_dialog_open: false,
            action_requires_device_approval: false,
            action_progress_asset_label: Arc::from(""),
            action_progress_icon_path: None,
            action_task_abort_handle: None,
            action_stop_available: false,
            action_stopped: false,
            action_command_tx: None,
            action_attempts: Vec::new(),
            action_current_gas_fee: None,
            action_fees_authorized: false,
            action_action_error: None,
            action_contract_address: None,
            next_derived_index: None,
            next_account_label_number: 1,
            error: None,
            send_error: None,
            shield_error: None,
            adding_account: false,
            hardware_derivation_status: HardwarePublicAccountDerivationStatus::Idle,
            hardware_confirmation_address: None,
            importing_account: false,
            sending: false,
            shielding: false,
            active_accounts_open: true,
            inactive_accounts_open: false,
            pending_global_delete_uuid: None,
        };
        let repair_cache_block_input = new_text_input(window, cx, "0 = deployment block");
        let tx_search_input = new_text_input(window, cx, "search tx hash");
        let mut platform_attention = platform_attention::PlatformAttentionState::new(window);
        platform_attention.sync_badge_count(0);
        platform_attention.clear_attention();
        let walletconnect = walletconnect::WalletConnectUiState::new(window, cx);
        let chain_select =
            cx.new(|cx| SelectState::new(chain_select_items, selected_chain_index, window, cx));
        let wallet_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        let root_weak = cx.weak_entity();
        let root_entity = cx.entity();
        let passphrase_open_ui = cx.new(|cx| PassphraseOpenUi::new(root_entity, window, cx));
        let broadcaster_preference_snapshot =
            Arc::new(RwLock::new(BroadcasterPreferences::default()));
        let preference_status_snapshot = Arc::clone(&broadcaster_preference_snapshot);
        let preference_root = root_weak.clone();
        let favorite_root = preference_root.clone();
        let banned_root = preference_root;
        monitor.update(cx, |monitor, cx| {
            monitor.set_preference_hooks(
                Some(broadcaster_monitor_gpui::BroadcasterPreferenceHooks::new(
                    move |address| {
                        preference_status_snapshot.read().map_or(
                            broadcaster_monitor_gpui::BroadcasterPreferenceStatus::Neutral,
                            |preferences| {
                                if broadcaster_preference_is_banned(&preferences, address) {
                                    broadcaster_monitor_gpui::BroadcasterPreferenceStatus::Banned
                                } else if broadcaster_preference_is_favorite(&preferences, address)
                                {
                                    broadcaster_monitor_gpui::BroadcasterPreferenceStatus::Favorite
                                } else {
                                    broadcaster_monitor_gpui::BroadcasterPreferenceStatus::Neutral
                                }
                            },
                        )
                    },
                    move |address, _window, cx| {
                        let _ = favorite_root.update(cx, |root, cx| {
                            root.toggle_favorite_broadcaster(&address, cx);
                        });
                    },
                    move |address, _window, cx| {
                        let _ = banned_root.update(cx, |root, cx| {
                            root.toggle_banned_broadcaster(&address, cx);
                        });
                    },
                )),
                cx,
            );
        });
        let utxo_table = cx.new(|cx| {
            TableState::new(
                UtxoDelegate::new(root_weak.clone(), tx_search_input.clone()),
                window,
                cx,
            )
        });
        let network_health = http.network_health();
        let tor_bridge_activity = http.tor_bridge_activity_snapshot();
        let sidebar_public_broadcaster_count =
            ethereum_weth_public_broadcaster_count(&monitor_state.read().fee_rows());
        let mut public_broadcaster_sort_seed = [0_u8; 32];
        rand::rng().fill(public_broadcaster_sort_seed.as_mut_slice());
        let mut anchor_refresh_rx = public_broadcaster_anchor_cache.subscribe_refreshes();
        let (root_shutdown, _) = watch::channel(false);
        let root = Self {
            selected_chain: initial_chain_id,
            options,
            vault_store,
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
            vault_state,
            wallet_setup_mode: WalletSetupMode::Choose,
            vault_error,
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
            hardware_wallet_creation_in_progress: false,
            hardware_wallet_creation_generation: 0,
            hardware_wallet_creation_intent: None,
            hardware_wallet_restore_account_index_input,
            hardware_wallet_restore_account_index_set: false,
            #[cfg(feature = "hardware")]
            hardware_profile_unlock: vault::HardwareProfileUnlockState::default(),
            #[cfg(feature = "hardware")]
            active_hardware_profile: None,
            #[cfg(feature = "hardware")]
            hardware_profile_password_input,
            #[cfg(feature = "hardware")]
            hardware_profile_label_input,
            #[cfg(feature = "hardware")]
            hardware_profile_recovery_start_input,
            #[cfg(feature = "hardware")]
            hardware_profile_recovery_count_input,
            #[cfg(feature = "hardware")]
            hardware_profile_exact_index_input,
            #[cfg(feature = "hardware")]
            trezor_app_passphrase_input,
            #[cfg(feature = "hardware")]
            trezor_passphrase_mode_focus,
            http,
            network_health,
            tor_bridge_activity,
            tor_download_rate: None,
            root_shutdown,
            network_status_popover_open: false,
            network_status_error: None,
            tor_exit_ip_query: TorExitIpQueryState::Idle,
            tor_exit_ip_query_generation: 0,
            tor_state_reset_confirming: false,
            prover_cache_build_progress: None,
            prover_cache_build_popover_open: false,
            prover_cache_build_monitor_active: false,
            prover_cache_build_completed: false,
            runtime,
            monitor_state,
            monitor_event_tx,
            waku_config,
            waku_runtime: None,
            waku_session_generation: 0,
            waku_stopping_generation: None,
            public_broadcaster_anchor_cache,
            public_broadcaster_anchor_refresh,
            monitor,
            logs,
            settings_editor,
            maintenance_controller: maintenance_controller.clone(),
            settings_error,
            active_activity: Activity::Wallet,
            active_wallet_tab: WalletTab::default(),
            sidebar_manually_collapsed: false,
            sidebar_narrow_expanded: false,
            sidebar_public_broadcaster_count,
            wallet_select: wallet_select.clone(),
            wallet_metadata: Vec::new(),
            wallet_options: Vec::new(),
            manage_wallets: ManageWalletsState::default(),
            key_export: KeyExportState::default(),
            manage_wallet_label_input,
            selected_wallet_id: None,
            active_wallet_generation: 0,
            wallet_switch_generation: 0,
            wallet_switch_delayed: false,
            ui_state,
            chain_select: chain_select.clone(),
            chain_states,
            pending_ppoi_validation_toast: None,
            private_pending_status_dialog_open: false,
            poi_artifact_cache_progress: BTreeMap::new(),
            poi_artifact_cache_retry_attempts: PoiArtifactCacheRetryAttempts::default(),
            wallet_sync_lifecycle: WalletSyncLifecycle::new(),
            wallet_sync_cleanup_tasks: Vec::new(),
            wallet_sync_lifecycle_shutdown_started: false,
            public_sync_cache_resetting,
            merkle_forest_cache_resetting,
            unlock_password_input,
            new_password_input,
            confirm_password_input,
            wallet_name_input,
            add_wallet_password_input,
            import_mnemonic_input,
            public_accounts: Vec::new(),
            address_book,
            private_address_book: Vec::new(),
            public_address_book: Vec::new(),
            broadcaster_preferences: BroadcasterPreferences::default(),
            broadcaster_preference_snapshot,
            broadcaster_preference_error: None,
            active_broadcaster_tab: BroadcasterActivityTab::default(),
            favorite_broadcaster_input: favorite_broadcaster_input.clone(),
            banned_broadcaster_input: banned_broadcaster_input.clone(),
            address_book_label_input,
            address_book_save_error: None,
            public_form,
            public_balance_snapshot: None,
            public_balance_error: None,
            public_balance_refreshing: false,
            public_balance_generation: 0,
            public_inactive_balance_error: None,
            public_inactive_balance_refreshing: false,
            public_inactive_balance_generation: 0,
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
            repair_cache_block_input,
            tx_search_input: tx_search_input.clone(),
            tx_search_query: Arc::from(""),
            walletconnect_attention_count: 0,
            walletconnect_window_active: window.is_window_active(),
            platform_attention,
            walletconnect,
            show_spent_utxos: false,
            local_pending_spent_clear_confirming: false,
            blocked_shield_rescue_lookup_generation: 0,
            blocked_shield_rescue_rows: BTreeMap::new(),
            blocked_shield_refunds_in_flight: BTreeSet::new(),
            utxo_table,
            focus_vault_input_on_render,
            focus_utxo_table_on_render: false,
            focus_public_account_search_on_render: false,
            logs_open: false,
            drawer_split: cx.new(|_| ResizableState::default()),
        };
        Self::start_auto_lock_monitor(window, cx);
        cx.observe_window_activation(window, |root, window, cx| {
            if window.is_window_active() {
                root.enforce_auto_lock(window, cx);
            }
            root.sync_walletconnect_attention_for_window(window);
        })
        .detach();
        cx.subscribe(&tx_search_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().trim().to_ascii_lowercase();
                this.tx_search_query = Arc::from(query);
                this.sync_utxo_table(cx);
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(
            &public_account_search_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_ascii_lowercase();
                    this.public_form.search_query = Arc::from(query);
                    cx.notify();
                }
            },
        )
        .detach();
        for input in [
            root.public_form.send_amount_input.clone(),
            root.public_form.shield_amount_input.clone(),
            root.public_form.send_gas_fee.max_fee_input.clone(),
            root.public_form.send_gas_fee.max_priority_fee_input.clone(),
            root.public_form.shield_gas_fee.max_fee_input.clone(),
            root.public_form
                .shield_gas_fee
                .max_priority_fee_input
                .clone(),
        ] {
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        for input in [
            root.public_form.advanced_send_to_input.clone(),
            root.public_form.advanced_send_value_input.clone(),
            root.public_form.advanced_send_data_input.clone(),
            root.public_form.send_gas_fee.max_fee_input.clone(),
            root.public_form.send_gas_fee.max_priority_fee_input.clone(),
        ] {
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate_advanced_public_send_estimate();
                    cx.notify();
                }
            })
            .detach();
        }
        cx.subscribe(
            &root.address_book.search_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_ascii_lowercase();
                    this.address_book.search_query = Arc::from(query);
                    cx.notify();
                }
            },
        )
        .detach();
        for input in [
            root.address_book.add_label_input.clone(),
            root.address_book.add_address_input.clone(),
            root.address_book.edit_label_input.clone(),
            root.address_book.edit_address_input.clone(),
        ] {
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        for (input, kind) in [
            (
                favorite_broadcaster_input,
                BroadcasterPreferenceListKind::Favorite,
            ),
            (
                banned_broadcaster_input,
                BroadcasterPreferenceListKind::Banned,
            ),
        ] {
            cx.subscribe_in(
                &input,
                window,
                move |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.add_broadcaster_preference_from_input(kind, window, cx);
                    }
                    InputEvent::Change => {
                        this.broadcaster_preference_error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }
        cx.subscribe_in(
            &chain_select,
            window,
            |this, _select, event: &SelectEvent<Vec<ChainSelectItem>>, window, cx| {
                let SelectEvent::Confirm(Some(chain_id)) = event else {
                    return;
                };
                this.select_chain(*chain_id, window, cx);
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur();
                    }
                });
            },
        )
        .detach();
        cx.subscribe_in(
            &root.walletconnect.uri_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.start_walletconnect_pairing_from_input(window, cx);
                }
                InputEvent::Change => {
                    this.walletconnect.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        cx.subscribe_in(
            &root.walletconnect.account_select,
            window,
            |this,
             _select,
             event: &SelectEvent<SearchableVec<walletconnect::WalletConnectAccountSelectItem>>,
             window,
             cx| {
                let SelectEvent::Confirm(Some(public_account_uuid)) = event else {
                    return;
                };
                this.set_walletconnect_selected_account(public_account_uuid.clone(), cx);
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur();
                    }
                });
            },
        )
        .detach();
        cx.subscribe_in(
            &wallet_select,
            window,
            |this, _select, event: &SelectEvent<SearchableVec<WalletSelectItem>>, window, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                this.select_wallet(value.as_ref(), window, cx);
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur();
                    }
                });
            },
        )
        .detach();
        cx.subscribe_in(
            &root.unlock_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.unlock_vault_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.new_password_input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::PressEnter { .. }) {
                    return;
                }
                let password_entered = !input.read(cx).value().trim().is_empty();
                let confirm_empty = this
                    .confirm_password_input
                    .read(cx)
                    .value()
                    .trim()
                    .is_empty();
                if password_entered && confirm_empty {
                    this.confirm_password_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window);
                } else {
                    this.create_vault_from_inputs(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.confirm_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.create_vault_from_inputs(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.wallet_name_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_default_hardware_wallet_setup(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.add_wallet_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_default_hardware_wallet_setup(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.hardware_wallet_restore_account_index_input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.submit_default_hardware_wallet_setup(window, cx);
                }
                InputEvent::Change => {
                    this.hardware_wallet_restore_account_index_set =
                        !input.read(cx).value().trim().is_empty();
                    this.vault_error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        #[cfg(feature = "hardware")]
        {
            cx.subscribe_in(
                &root.hardware_profile_password_input,
                window,
                |this, _input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.unlock_hardware_profile_from_dialog(window, cx);
                    }
                },
            )
            .detach();
            for input in [
                root.hardware_profile_label_input.clone(),
                root.hardware_profile_recovery_start_input.clone(),
                root.hardware_profile_recovery_count_input.clone(),
                root.hardware_profile_exact_index_input.clone(),
            ] {
                cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.hardware_profile_unlock.error = None;
                        cx.notify();
                    }
                })
                .detach();
            }
            cx.subscribe_in(
                &root.trezor_app_passphrase_input,
                window,
                |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.submit_trezor_profile_passphrase_input(window, cx);
                    }
                    InputEvent::Change => {
                        this.hardware_profile_unlock.error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }
        cx.subscribe(
            &root.repair_cache_block_input,
            |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.repair_wallet_cache_from_input(cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.manage_wallet_label_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_wallet_label_edit(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe(&root.utxo_table, |_, table, event: &TableEvent, cx| {
            if let TableEvent::ColumnWidthsChanged(widths) = event {
                table.update(cx, |table, cx| {
                    table.delegate_mut().set_column_widths(widths);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while monitor_event_rx.changed().await.is_ok() {
                if this
                    .update(cx, |root, cx| {
                        let current_public_broadcaster_count =
                            ethereum_weth_public_broadcaster_count(&root.monitor_fee_rows());
                        let public_broadcaster_count_changed = root
                            .sidebar_public_broadcaster_count
                            != current_public_broadcaster_count;
                        root.sidebar_public_broadcaster_count = current_public_broadcaster_count;
                        if root
                            .send_forms
                            .values()
                            .any(|form| form.delivery_mode == DeliveryMode::PublicBroadcaster)
                            || root
                                .unshield_forms
                                .values()
                                .any(|form| form.delivery_mode == DeliveryMode::PublicBroadcaster)
                            || public_broadcaster_count_changed
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while anchor_refresh_rx.changed().await.is_ok() {
                if this.update(cx, |_root, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.subscribe_in(
            &root.public_form.add_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_public_derived_account_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.public_form.import_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.import_public_account_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &root.public_form.edit_label_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.update_selected_public_account_label(window, cx);
                }
            },
        )
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(UTXO_AGE_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |root, cx| {
                        if should_refresh_utxo_ages(
                            root.active_activity,
                            root.active_wallet_tab,
                            root.chain_states
                                .get(&root.selected_chain)
                                .is_some_and(|state| state.snapshot().is_some()),
                        ) {
                            root.utxo_table.update(cx, |_table, cx| cx.notify());
                        }
                        if root.private_pending_status_dialog_open
                            && root.private_pending_status_has_shield_timer()
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(UNSHIELD_SPINNER_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |root, cx| {
                        if root.send_forms.values().any(|form| {
                            form.generating || form.cost_estimate_pending || form.estimating_cost
                        }) || root.unshield_forms.values().any(|form| {
                            form.generating || form.cost_estimate_pending || form.estimating_cost
                        }) {
                            root.unshield_spinner_tick = root.unshield_spinner_tick.wrapping_add(1);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let public_balance_refresh_interval = root.public_balance_refresh_interval;
        cx.spawn(async move |this, cx| {
            let interval = public_balance_refresh_interval;
            loop {
                cx.background_executor().timer(interval).await;
                if this
                    .update(cx, |root, cx| {
                        if root.active_wallet_tab == WalletTab::Public {
                            root.schedule_public_balance_refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        root.spawn_network_health_monitor(cx);
        root.spawn_tor_bridge_activity_sampler(cx);
        root
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
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

pub(super) fn new_masked_input<T>(
    window: &mut Window,
    cx: &mut Context<'_, T>,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(true)
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
