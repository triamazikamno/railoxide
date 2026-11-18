use super::super::account_select::public_account_walletconnect_label;
use super::super::fee::{
    WalletConnectFeeState, WalletConnectFeeStatus, WalletConnectReviewedFeeProjection,
    validate_walletconnect_reviewed_fee_pairing, walletconnect_fee_retry_action_enabled,
    walletconnect_fee_retrying, walletconnect_fee_state_projection,
    walletconnect_request_can_simulate, walletconnect_request_fee_eligible,
    walletconnect_request_raw_gas,
};
use super::super::helpers::{
    WALLETCONNECT_NETWORK_FEE_LABEL, WALLETCONNECT_RAW_REQUEST_LABEL,
    WALLETCONNECT_TRANSACTION_DETAILS_LABEL, WalletConnectRequestExpiryStatus,
    parse_caip2_chain_id, walletconnect_transaction_selector,
};
use super::super::intent::{
    WalletConnectAmount, WalletConnectHeroSummary, WalletConnectIntentAction,
    WalletConnectIntentContext, WalletConnectIntentView, WalletConnectParty,
    WalletConnectPartyRole, WalletConnectRisk, build_walletconnect_intent,
    classify_walletconnect_fee_significance, walletconnect_approximate_usd_label,
    walletconnect_moving_usd_micro_value, walletconnect_party_address_label,
    walletconnect_party_badge_label, walletconnect_party_role_label,
    walletconnect_selected_account_provenance_visible, walletconnect_should_render_token_contract,
    walletconnect_simulation_risk, walletconnect_token_contract_recognition,
};
use super::super::render::walletconnect_disclosure_content;
use super::*;
use crate::root::gas_fee::{
    Eip1559GasFeeEditorState, Eip1559GasFeeTarget, format_gwei, render_eip1559_gas_fee_editor,
};
use crate::root::public_action::format_gas_limit;
use alloy::primitives::Address;
use gpui::relative;
use gpui_component::collapsible::Collapsible;
use railgun_ui::format_usd_micro_value;
use ui::controls::{app_button_label, app_muted_shimmer_text};

mod intent_render;
mod request_approval;
mod request_fee_details;
mod request_render;
mod request_revalidation;

use intent_render::{
    append_walletconnect_simulation_risk, render_walletconnect_address_row,
    render_walletconnect_intent_card, render_walletconnect_intent_risk,
    render_walletconnect_request_provenance,
};
#[cfg(test)]
use request_fee_details::walletconnect_dapp_gas_price;
use request_fee_details::{
    parse_display_chain_id, render_walletconnect_network_fee,
    render_walletconnect_request_disclosure, render_walletconnect_transaction_details,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalletConnectRequestFooterActionState {
    reject_enabled: bool,
    approve_enabled: bool,
    approve_danger: bool,
}

const fn walletconnect_request_footer_action_state(
    in_flight: bool,
    locally_expired: bool,
    intent_context_available: bool,
    unlimited_allowance: bool,
    hardware_request: bool,
    hardware_typed_data_hash_fallback: bool,
) -> WalletConnectRequestFooterActionState {
    WalletConnectRequestFooterActionState {
        reject_enabled: !in_flight,
        approve_enabled: intent_context_available && !in_flight && !locally_expired,
        approve_danger: intent_context_available
            && unlimited_allowance
            && !in_flight
            && !hardware_request
            && !hardware_typed_data_hash_fallback,
    }
}

pub(in crate::root::walletconnect) fn gateway_pending_request(
    approval: &wallet_ops::gateway::GatewayApprovalRequest,
    rpc_reads: wallet_ops::DappRpcReadClient,
    operation: wallet_ops::OperationHttpClientSlot,
) -> Option<WalletConnectRequestUi> {
    approval.control.ensure_current().ok()?;
    let key = format!("gateway:{}", approval.id);
    let chain_id = format!("eip155:{}", approval.chain_id);
    let remaining = approval
        .deadline
        .saturating_duration_since(tokio::time::Instant::now());
    let expiry = current_unix_seconds()
        .saturating_add(remaining.as_secs())
        .saturating_add(u64::from(remaining.subsec_nanos() != 0));
    let item = approval.parsed.desktop_approval(
        0,
        &key,
        "Browser dapp",
        &chain_id,
        approval
            .authorization
            .as_ref()
            .map(|authorization| authorization.account.address),
        Some(expiry),
    );
    Some(WalletConnectRequestUi {
        key,
        review_token: walletconnect_request_id_seed(),
        binding: DappRequestBinding {
            account: approval
                .authorization
                .as_ref()
                .map(|authorization| DappRequestAccount {
                    public_account_uuid: authorization.permission.public_account_uuid.clone(),
                    public_account_scope: authorization.permission.public_account_scope.clone(),
                    owning_private_wallet_uuid: authorization
                        .permission
                        .owning_private_wallet_uuid
                        .clone(),
                }),
            peer_name: approval.origin.paired_peer_id()?.to_owned(),
            peer_url: approval.origin.web_origin()?.as_str().to_owned(),
        },
        session_identity: DappSessionIdentity {
            transport: "gateway",
            id: approval.id.clone(),
        },
        parsed: approval.parsed.clone(),
        item,
        account_source: approval
            .authorization
            .as_ref()
            .map(|authorization| authorization.account.source),
        request_control: Some(approval.control.clone()),
        rpc_reads: approval.authorization.as_ref().map(|_| rpc_reads),
        operation: Some(operation),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use std::sync::Arc;

    fn fee_state(
        request_key: &str,
        status: WalletConnectFeeStatus,
        error: Option<&str>,
    ) -> WalletConnectFeeState {
        WalletConnectFeeState {
            request_key: Arc::from(request_key),
            review_token: 0,
            payload_fingerprint: B256::ZERO,
            request_generation: 0,
            dialog_generation: 0,
            editor_generation: 0,
            expiry_timestamp: None,
            status,
            error: error.map(Arc::from),
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        }
    }

    #[test]
    fn dapp_gas_price_formats_safe_values_as_gwei_without_truncating_large_values() {
        assert_eq!(
            walletconnect_dapp_gas_price(U256::from(1_500_000_000_u64)),
            "1.5 gwei (not used)"
        );
        let oversized = U256::from(u128::MAX) + U256::from(1_u8);
        assert_eq!(
            walletconnect_dapp_gas_price(oversized),
            format!("{oversized} wei (not used)")
        );
    }

    #[test]
    fn footer_disables_approve_for_expiry_or_missing_intent_context() {
        let expired =
            walletconnect_request_footer_action_state(false, true, true, false, false, false);
        assert!(expired.reject_enabled);
        assert!(!expired.approve_enabled);

        let missing_context =
            walletconnect_request_footer_action_state(false, false, false, false, false, false);
        assert!(missing_context.reject_enabled);
        assert!(!missing_context.approve_enabled);
    }

    #[test]
    fn footer_disables_both_actions_while_request_is_in_flight() {
        let state =
            walletconnect_request_footer_action_state(true, false, true, true, false, false);

        assert!(!state.reject_enabled);
        assert!(!state.approve_enabled);
        assert!(!state.approve_danger);
    }

    #[test]
    fn request_scoped_fee_risk_enrichment_adds_revert_but_not_unavailable() {
        let revert = fee_state(
            "request-key",
            WalletConnectFeeStatus::WouldRevert,
            Some("execution reverted"),
        );
        let mut risks = Vec::new();
        append_walletconnect_simulation_risk("request-key", Some(&revert), &mut risks);
        assert_eq!(
            risks,
            vec![WalletConnectRisk::WouldRevert {
                reason: "execution reverted".to_owned(),
            }]
        );

        let other_request = fee_state(
            "other-request-key",
            WalletConnectFeeStatus::WouldRevert,
            Some("execution reverted"),
        );
        risks.clear();
        append_walletconnect_simulation_risk("request-key", Some(&other_request), &mut risks);
        assert_eq!(risks, [] as [WalletConnectRisk; 0]);

        let unavailable = fee_state(
            "request-key",
            WalletConnectFeeStatus::UnavailableFailed,
            Some("provider unavailable"),
        );
        risks.clear();
        append_walletconnect_simulation_risk("request-key", Some(&unavailable), &mut risks);
        assert_eq!(risks, [] as [WalletConnectRisk; 0]);
    }
}
