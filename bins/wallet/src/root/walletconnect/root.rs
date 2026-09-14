use super::{
    account_select::{
        normalized_walletconnect_account_uuid, public_account_walletconnect_label,
        sync_walletconnect_account_select_entity,
    },
    helpers::{
        current_unix_seconds, ensure_walletconnect_chain_id_enabled,
        expired_walletconnect_pairing_topics, walletconnect_enabled_chain_ids,
        walletconnect_error_message, walletconnect_hardware_typed_data_mode_for_request,
        walletconnect_is_transient_relay_error, walletconnect_namespace_account_support,
        walletconnect_pairing_expired, walletconnect_proposal_rejection_reason,
        walletconnect_proposal_requests_required_typed_data,
        walletconnect_relay_request_was_not_sent, walletconnect_request_expiry_status,
        walletconnect_request_id_seed, walletconnect_request_key_log_label,
        walletconnect_session_expired, walletconnect_session_has_expiring_lifecycle,
        walletconnect_session_uuid, walletconnect_session_visible_in_management,
        walletconnect_topic_log_label, walletconnect_validate_pending_request_expiry,
    },
    relay::{
        encode_walletconnect_response_message, execute_walletconnect_approval_relay_steps,
        execute_walletconnect_relay_steps, process_walletconnect_relay_output,
        stop_stale_walletconnect_relay_workers, walletconnect_active_sessions,
        walletconnect_active_sessions_for_relay_client, walletconnect_client_from_identity,
        walletconnect_relay_target_topics, walletconnect_relay_worker_loop,
        walletconnect_session_request_failure_from_error,
    },
    render::{
        approved_chain_display_item, format_unix_seconds, render_walletconnect_approval_stepper,
        walletconnect_approved_chain_chip, walletconnect_approved_chains_row,
        walletconnect_completed_tx_hash_row, walletconnect_kv_element_row, walletconnect_kv_row,
        walletconnect_lifecycle_label, walletconnect_logo_with_badges,
        walletconnect_metadata_block, walletconnect_privacy_notices, walletconnect_raw_details,
        walletconnect_subpanel, walletconnect_title_row,
        walletconnect_unresolved_public_account_label,
    },
    requests::{
        approve_walletconnect_request_task, expired_walletconnect_request_keys,
        first_walletconnect_pending_request_key, hardware_walletconnect_notice,
        next_walletconnect_auto_open_request_key,
        walletconnect_request_authorization_summary_with_fee, walletconnect_request_dialog_nav,
        walletconnect_request_matches_review_token, walletconnect_request_should_queue,
    },
    *,
};

#[cfg(feature = "hardware")]
use super::helpers::walletconnect_proposal_requests_hardware_typed_data;
#[cfg(feature = "hardware")]
use super::render::walletconnect_trezor_app_passphrase_input;

mod attention;
mod connection;
mod pairing;
mod policy;
mod relay_lifecycle;
mod request_actions;
mod request_dialog;
mod session_dialogs;
mod toolbar;

#[cfg(test)]
pub(super) use request_actions::gateway_pending_request;
