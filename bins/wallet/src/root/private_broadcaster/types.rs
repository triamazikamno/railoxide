use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Pixels, px};
use wallet_ops::{
    DesktopSelfBroadcastResult, PublicBroadcasterCostEstimate, PublicBroadcasterSubmissionResult,
    SelfBroadcastAttemptInfo, SelfBroadcastCommandSender, SponsoredSelfBroadcastCommandSender,
    SponsoredSelfBroadcastSessionOutcome, TransactionGenerationStage,
};

use crate::assets::WalletIconSource;
use crate::root::public_action::PublicActionStepStatus;
use crate::root::{DeliveryFormKind, UnshieldAssetKey};

pub(super) const PRIVATE_BROADCASTER_PROGRESS_STAGES: [TransactionGenerationStage; 6] = [
    TransactionGenerationStage::SelectingPrivateNotes,
    TransactionGenerationStage::ProvingTransaction,
    TransactionGenerationStage::EstimatingBroadcasterFee,
    TransactionGenerationStage::GeneratingPoiProofs,
    TransactionGenerationStage::PublishingToBroadcaster,
    TransactionGenerationStage::WaitingForBroadcasterResponse,
];

pub(super) const SELF_BROADCAST_PROGRESS_STAGES: [TransactionGenerationStage; 6] = [
    TransactionGenerationStage::SelectingPrivateNotes,
    TransactionGenerationStage::ProvingTransaction,
    TransactionGenerationStage::GeneratingPoiProofs,
    TransactionGenerationStage::EstimatingSelfBroadcastGas,
    TransactionGenerationStage::SigningSelfBroadcast,
    TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
];

pub(super) const SELF_BROADCAST_UNSHIELD_PROGRESS_STAGES: [TransactionGenerationStage; 5] = [
    TransactionGenerationStage::SelectingPrivateNotes,
    TransactionGenerationStage::ProvingTransaction,
    TransactionGenerationStage::EstimatingSelfBroadcastGas,
    TransactionGenerationStage::SigningSelfBroadcast,
    TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
];

pub(super) const SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH: Pixels = px(460.0);
pub(super) const PUBLIC_BROADCASTER_STOP_RESEND_THRESHOLD: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PrivateSubmissionProgressFlow {
    PublicBroadcaster,
    SelfBroadcast,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PrivateBroadcasterProgressStepState {
    pub(in crate::root) stage: TransactionGenerationStage,
    pub(in crate::root) status: PublicActionStepStatus,
    pub(in crate::root) message: Option<Arc<str>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PrivateBroadcasterClosedActiveProgress {
    pub(in crate::root) flow: PrivateSubmissionProgressFlow,
    pub(in crate::root) generation_id: u64,
    pub(in crate::root) requires_device_approval: bool,
    pub(in crate::root) step: PrivateBroadcasterProgressStepState,
}

pub(in crate::root) struct PrivateBroadcasterProgressState {
    pub(in crate::root) gateway_execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
    pub(in crate::root) stealth_account:
        Option<crate::root::stealth_accounts::StealthAccountTarget>,
    pub(in crate::root) flow: PrivateSubmissionProgressFlow,
    pub(in crate::root) kind: DeliveryFormKind,
    pub(in crate::root) key: UnshieldAssetKey,
    pub(in crate::root) generation_id: u64,
    pub(in crate::root) asset_label: Arc<str>,
    pub(in crate::root) icon_path: Option<WalletIconSource>,
    pub(in crate::root) recipient: Arc<str>,
    pub(in crate::root) recipient_output: Option<Arc<str>>,
    pub(in crate::root) gas_payer: Option<Arc<str>>,
    pub(in crate::root) requires_device_approval: bool,
    pub(in crate::root) sponsored_funding: bool,
    pub(in crate::root) steps: Vec<PrivateBroadcasterProgressStepState>,
    pub(in crate::root) estimate: Option<PublicBroadcasterCostEstimate>,
    pub(in crate::root) result: Option<PublicBroadcasterSubmissionResult>,
    pub(in crate::root) self_broadcast_result: Option<DesktopSelfBroadcastResult>,
    pub(in crate::root) self_broadcast_command_tx: Option<SelfBroadcastCommandSender>,
    pub(in crate::root) sponsored_self_broadcast_command_tx:
        Option<SponsoredSelfBroadcastCommandSender>,
    pub(in crate::root) sponsored_self_broadcast_outcome:
        Option<SponsoredSelfBroadcastSessionOutcome>,
    pub(in crate::root) self_broadcast_attempts: Vec<SelfBroadcastAttemptInfo>,
    pub(in crate::root) self_broadcast_current_gas_fee: Option<(u128, u128)>,
    pub(in crate::root) self_broadcast_action_error: Option<Arc<str>>,
    pub(in crate::root) public_broadcaster_response_timeout: Option<Duration>,
    pub(in crate::root) public_broadcaster_republish_interval: Option<Duration>,
    pub(in crate::root) public_broadcaster_wait_started_at: Option<Instant>,
    pub(in crate::root) task_abort_handle: Option<tokio::task::AbortHandle>,
    pub(in crate::root) stop_available: bool,
    pub(in crate::root) stopped: bool,
    pub(in crate::root) error: Option<Arc<str>>,
    pub(in crate::root) dialog_open: bool,
    pub(in crate::root) stage_seen: bool,
}

impl PrivateBroadcasterProgressState {
    pub(in crate::root) fn accepts_update(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
    ) -> bool {
        self.kind == kind
            && self.key == key
            && self.generation_id == generation_id
            && self.result.is_none()
            && self.self_broadcast_result.is_none()
            && self.sponsored_self_broadcast_outcome.is_none()
            && self.error.is_none()
            && !self.stopped
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SelfBroadcastGasRetryKind {
    RetryStep,
    RetryEstimate,
    SpeedUp,
}

pub(super) const fn private_broadcaster_dialog_title_action(
    kind: DeliveryFormKind,
) -> &'static str {
    match kind {
        DeliveryFormKind::Send => "Send via broadcaster",
        DeliveryFormKind::Unshield => "Unshield via broadcaster",
    }
}

pub(super) const fn private_submission_dialog_title_action(
    flow: PrivateSubmissionProgressFlow,
    kind: DeliveryFormKind,
) -> &'static str {
    match flow {
        PrivateSubmissionProgressFlow::PublicBroadcaster => {
            private_broadcaster_dialog_title_action(kind)
        }
        PrivateSubmissionProgressFlow::SelfBroadcast => match kind {
            DeliveryFormKind::Send => "Self-broadcast send",
            DeliveryFormKind::Unshield => "Self-broadcast unshield",
        },
    }
}
