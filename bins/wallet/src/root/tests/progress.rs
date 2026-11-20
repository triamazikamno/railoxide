use super::*;
use crate::root::private_broadcaster::{
    BroadcasterPreferenceControl, broadcaster_preference_control,
    cancel_private_broadcaster_progress, gateway_self_broadcast_result,
    private_broadcaster_progress_dialog_close_behavior, private_broadcaster_progress_is_terminal,
    private_broadcaster_route_card, request_sponsored_session_cancel,
    spawn_sponsored_abort_watchdog, sponsored_stop_uses_session_command,
};
use crate::root::public_action::{
    PublicActionGasRetryKind, public_action_discard_attempt_available,
    public_action_error_retry_kind, public_action_step_detail_for_context, public_action_step_id,
};
use wallet_ops::{
    DesktopSelfBroadcastResult, SelfBroadcastAttemptInfo, SponsoredSelfBroadcastSessionOutcome,
    TxReceiptOutput,
};

fn test_self_broadcast_result(status: bool) -> DesktopSelfBroadcastResult {
    DesktopSelfBroadcastResult {
        chain_id: 1,
        public_account_uuid: "public-account".to_string(),
        gas_payer: Address::from([0x22; 20]),
        gas_limit: 21_000,
        rpc_gas_price: 1,
        max_fee_per_gas: 2,
        max_priority_fee_per_gas: 1,
        estimated_native_gas_cost: U256::from(21_000),
        live_native_balance: U256::from(42_000),
        tx: wallet_ops::SelfBroadcastTxOutcome::Receipt(TxReceiptOutput {
            tx_hash: "0xabc".to_string(),
            status,
            block_number: 10,
            gas_used: 21_000,
            contract_address: None,
        }),
        attempts: Vec::new(),
        native_top_up: None,
    }
}

mod broadcaster_progress;
mod public_action_steps;
mod self_broadcast_results;
mod self_broadcast_stages;
mod stop_footers_and_status;
