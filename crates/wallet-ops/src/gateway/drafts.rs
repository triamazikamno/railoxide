//! Draft messages and transaction progress shared with the desktop owner.
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::{
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDraftKind {
    Send,
    Shield,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayDraftFee {
    Slow,
    Normal,
    Fast,
    Custom {
        max_fee_gwei: String,
        priority_fee_gwei: String,
    },
}

/// The approval amount of a Railoxide shield that may sign an ERC-20 approval. Mimic Railway
/// signs its own unlimited policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDraftApproval {
    #[default]
    Exact,
    Unlimited,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayDraftInput {
    pub account: String,
    #[serde(with = "railgun_ui::chain_id")]
    pub chain_id: u64,
    pub kind: GatewayDraftKind,
    pub asset: String,
    pub amount: String,
    pub recipient: String,
    pub address_book_entry: Option<String>,
    pub fee: GatewayDraftFee,
    pub mimic_railway: bool,
    #[serde(default)]
    pub approval: GatewayDraftApproval,
    pub max: bool,
}

/// Public inputs keep their original wire representation. Private kinds cannot parse as Public.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GatewayDraftPayload {
    Public(GatewayDraftInput),
    Private(super::GatewayPrivateDraftInput),
}

impl GatewayDraftPayload {
    #[must_use]
    pub const fn public(&self) -> Option<&GatewayDraftInput> {
        match self {
            Self::Public(input) => Some(input),
            Self::Private(_) => None,
        }
    }

    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        match self {
            Self::Public(input) => input.chain_id,
            Self::Private(input) => input.chain_id,
        }
    }
}

impl From<GatewayDraftInput> for GatewayDraftPayload {
    fn from(input: GatewayDraftInput) -> Self {
        Self::Public(input)
    }
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GatewayDraftEstimatePayload {
    Public(GatewayDraftEstimate),
    Private(super::GatewayPrivateDraftEstimate),
}

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayDraftCommand {
    Create {
        request_id: String,
        input: GatewayDraftPayload,
    },
    Update {
        draft_id: String,
        revision: u64,
        input: GatewayDraftPayload,
    },
    Submit {
        draft_id: String,
        revision: u64,
    },
    Cancel {
        draft_id: String,
    },
    PrivatePicker {
        draft_id: String,
        revision: u64,
        view_id: String,
        open: bool,
        query: String,
    },
    PrivateControl {
        draft_id: String,
        execution_id: String,
        control: super::GatewayPrivateDraftControl,
    },
    Dismiss {
        draft_id: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDraftStatus {
    Editing,
    Estimating,
    Ready,
    Attention,
    InProgress,
    Done,
    Failed,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct GatewayDraftRecipient {
    pub id: String,
    pub label: String,
    pub address: String,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
pub struct GatewayDraftEstimate {
    pub amount: String,
    pub amount_label: String,
    pub amount_value: Option<String>,
    pub max_amount_label: Option<String>,
    pub recipient: Option<String>,
    pub gas_limit: Option<String>,
    pub expected_gas_cost: String,
    pub maximum_gas_cost: String,
    pub show_maximum_gas_cost: bool,
    pub protocol_fee: Option<String>,
    pub protocol_fee_label: Option<String>,
}

/// Desktop gas price hints remain available while transaction inputs are incomplete.
#[derive(Clone, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayDraftGasQuote {
    pub max_fee_gwei: String,
    pub priority_fee_gwei: String,
}

impl GatewayDraftGasQuote {
    #[must_use]
    pub const fn new(max_fee_gwei: String, priority_fee_gwei: String) -> Self {
        Self {
            max_fee_gwei,
            priority_fee_gwei,
        }
    }
}

#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct GatewayDraftView {
    #[serde(skip)]
    pub peer_id: String,
    pub draft_id: String,
    pub request_id: String,
    pub revision: u64,
    pub input: GatewayDraftPayload,
    pub status: GatewayDraftStatus,
    pub estimate: Option<GatewayDraftEstimatePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_progress: Option<super::GatewayPrivateDraftProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_options: Option<super::GatewayPrivateDraftOptions>,
    pub gas_quote: Option<GatewayDraftGasQuote>,
    pub recipients: Vec<GatewayDraftRecipient>,
    pub step_label: String,
    pub message: String,
    pub warning: bool,
    pub can_cancel: bool,
    pub can_retry: bool,
}

/// The transaction task retains this handle after its browser disconnects.
/// It contains public progress only; signing capabilities stay in the existing task.
#[derive(Clone)]
pub struct GatewayDraftExecution(Arc<Mutex<ExecutionState>>);

struct ExecutionState {
    started: bool,
    review_approved: bool,
    generation: Option<u64>,
    cancelled: bool,
    handed_off: bool,
    status: GatewayDraftStatus,
    step: String,
    message: String,
    warning: bool,
    private: Option<super::GatewayPrivateDraftProgress>,
    /// The paired browser the draft came from, as its journal entry names it.
    origin: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GatewayDraftProgress {
    pub status: GatewayDraftStatus,
    pub step_label: String,
    pub message: String,
    pub warning: bool,
    pub can_retry: bool,
    pub private: Option<super::GatewayPrivateDraftProgress>,
}

impl Default for GatewayDraftExecution {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(ExecutionState {
            started: false,
            review_approved: false,
            generation: None,
            cancelled: false,
            handed_off: false,
            status: GatewayDraftStatus::Attention,
            step: "Approve in the desktop app".into(),
            message: "Review this transaction in the desktop app.".into(),
            warning: false,
            private: None,
            origin: None,
        })))
    }
}

impl GatewayDraftExecution {
    #[must_use]
    pub fn private(execution_id: String) -> Self {
        let execution = Self::default();
        let private = super::GatewayPrivateDraftProgress {
            execution_id,
            ..Default::default()
        };
        execution
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .private = Some(private);
        execution
    }

    /// Names the paired browser the draft came from, for the submission's journal entry.
    #[must_use]
    pub fn with_origin(self, origin: String) -> Self {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .origin = Some(origin);
        self
    }

    #[must_use]
    pub fn origin(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .origin
            .clone()
    }

    #[must_use]
    pub fn same_execution(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[must_use]
    pub fn has_review_approval(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .review_approved
    }

    /// Only the native operation owner supplies this display projection.
    pub fn update_private(
        &self,
        status: GatewayDraftStatus,
        step: String,
        message: String,
        warning: bool,
        private: super::GatewayPrivateDraftProgress,
    ) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .private
            .as_ref()
            .is_none_or(|current| current.execution_id != private.execution_id)
            || state.cancelled
            || (matches!(
                state.status,
                GatewayDraftStatus::Done | GatewayDraftStatus::Failed
            ) && state.status != status)
        {
            return;
        }
        state.handed_off |= private.result == Some(super::GatewayPrivateDraftResult::Submitted);
        state.private = Some(private);
        state.status = status;
        state.step = step;
        state.message = message;
        state.warning = warning;
    }

    #[must_use]
    pub fn approve_review(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled
            || state.started
            || matches!(
                state.status,
                GatewayDraftStatus::Failed | GatewayDraftStatus::Done
            )
        {
            return false;
        }
        state.review_approved = true;
        true
    }

    /// Hold submission until preparation has been checked against the approved terms.
    pub fn require_prepared_review(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled
            || state.started
            || !state.review_approved
            || matches!(
                state.status,
                GatewayDraftStatus::Failed | GatewayDraftStatus::Done
            )
        {
            return false;
        }
        state.review_approved = false;
        state.status = GatewayDraftStatus::Attention;
        state.step = "Preparing account".into();
        state.message =
            "Checking the prepared transaction against your approved fee limits.".into();
        true
    }

    /// Revalidation failure permits an explicit draft retry, never replay of started work.
    pub fn reject_private_review(&self, message: String) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.started
            && !state.cancelled
            && state.status == GatewayDraftStatus::Attention
            && let Some(private) = state.private.as_mut()
        {
            private.result = Some(super::GatewayPrivateDraftResult::Failed);
            state.review_approved = false;
            state.status = GatewayDraftStatus::Failed;
            state.step = "Transaction needs updating".into();
            state.message = message;
            state.warning = true;
        }
    }

    pub fn bind_generation(&self, generation: u64) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation = Some(generation);
    }

    #[must_use]
    pub fn generation(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    /// Atomic admission also rejects late approval of a canceled draft.
    #[must_use]
    pub fn start(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.review_approved
            || state.started
            || state.cancelled
            || matches!(
                state.status,
                GatewayDraftStatus::Failed | GatewayDraftStatus::Done
            )
        {
            return false;
        }
        state.started = true;
        state.status = GatewayDraftStatus::InProgress;
        state.step = "Preparing transaction".into();
        state.message = "The desktop app is preparing the transaction.".into();
        true
    }

    /// Canceling a review never stops a transaction task that has already started.
    pub fn cancel_review(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.started || state.review_approved || state.status == GatewayDraftStatus::Failed {
            return;
        }
        state.cancelled = true;
        state.status = GatewayDraftStatus::Failed;
        state.step = "Canceled".into();
        state.message = "The transaction was not submitted.".into();
        if let Some(private) = &mut state.private {
            private.result = Some(super::GatewayPrivateDraftResult::Cancelled);
        }
    }

    /// A hardware prompt after an approved review, private or public, may be cancelled.
    /// The native dialog owner calls this only for an explicit dismissal, before generation.
    pub fn cancel_authorization(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.started
            || matches!(
                state.status,
                GatewayDraftStatus::Done | GatewayDraftStatus::Failed
            )
        {
            return;
        }
        state.cancelled = true;
        state.review_approved = false;
        state.status = GatewayDraftStatus::Failed;
        state.step = "Canceled".into();
        state.message = "The transaction was not submitted.".into();
        if let Some(private) = &mut state.private {
            private.result = Some(super::GatewayPrivateDraftResult::Cancelled);
        }
    }

    /// Retain the last result after the native owner releases its task and controls.
    pub fn retire_private_owner(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(private) = &mut state.private else {
            return;
        };
        private.stop = false;
        private.stop_retries = false;
        private.stop_waiting = false;
        private.ban = false;
        private.favorite = false;
        if private.result.is_none() {
            private.result = Some(super::GatewayPrivateDraftResult::Stopped);
            state.status = GatewayDraftStatus::Failed;
            state.step = "Stopped".into();
            state.message = "Desktop processing stopped. A published transaction may still be submitted; check history before creating another transaction.".into();
            state.warning = true;
        }
        state.cancelled = true;
    }

    pub fn stopped(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        state.status = GatewayDraftStatus::Failed;
        state.step = "Stopped".into();
        state.message = "Stopped before the final transaction handoff. Check the desktop transaction history for any earlier steps.".into();
    }

    pub fn progress(&self, update: &PublicActionProgressUpdate) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if update.tx_hash.is_some() {
            state.handed_off = true;
        }
        if state.cancelled
            || matches!(
                state.status,
                GatewayDraftStatus::Attention
                    | GatewayDraftStatus::Done
                    | GatewayDraftStatus::Failed
            )
        {
            return;
        }
        state.step = step_label(update.step).into();
        if update.status == PublicActionProgressStatus::Pending {
            state.message = "The desktop app is processing this step.".into();
        }
    }

    pub fn event(&self, event: &PublicActionSessionEvent) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            event,
            PublicActionSessionEvent::AttemptHandoff { .. }
                | PublicActionSessionEvent::AttemptSubmitted { .. }
        ) {
            state.handed_off = true;
            if state.cancelled {
                state.message =
                    "A transaction may have been submitted. Check its status in the desktop app."
                        .into();
            }
        }
        if state.cancelled
            || matches!(
                state.status,
                GatewayDraftStatus::Done | GatewayDraftStatus::Failed
            )
        {
            return;
        }
        match event {
            PublicActionSessionEvent::AttemptHandoff { .. }
            | PublicActionSessionEvent::AttemptSubmitted { .. } => {
                state.handed_off = true;
                state.status = GatewayDraftStatus::InProgress;
                state.message =
                    "The desktop app is submitting and observing the transaction.".into();
            }
            PublicActionSessionEvent::FeeAuthorizationRequired { step, .. } => {
                state.status = GatewayDraftStatus::Attention;
                state.warning = false;
                state.step = step_label(*step).into();
                state.message = "Review the fees for this step in the desktop app.".into();
            }
            PublicActionSessionEvent::HardwareApprovalStarted => {
                state.status = GatewayDraftStatus::Attention;
                state.warning = false;
                state.message =
                    "Approve this step on your hardware wallet through the desktop app.".into();
            }
            PublicActionSessionEvent::HardwareApprovalCompleted => {
                state.status = GatewayDraftStatus::InProgress;
                state.message = "The desktop app is processing this step.".into();
            }
            PublicActionSessionEvent::AttemptRejected { .. }
            | PublicActionSessionEvent::HardwareApprovalFailed { .. }
            | PublicActionSessionEvent::StepFailed { .. } => {
                state.status = GatewayDraftStatus::Attention;
                state.warning = true;
                state.message = "This step needs attention in the desktop app.".into();
            }
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { .. } => {}
        }
    }

    pub fn finish(&self, success: bool) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled {
            return;
        }
        state.status = if success {
            GatewayDraftStatus::Done
        } else {
            GatewayDraftStatus::Failed
        };
        state.step = if success {
            "Complete"
        } else {
            "Transaction failed"
        }
        .into();
        state.message = if success {
            "The transaction completed."
        } else if state.started || state.handed_off {
            "A transaction may have been submitted. Check its status in the desktop app before taking further action."
        } else { "The transaction failed before submission. Review it before trying again." }.into();
    }

    #[must_use]
    pub fn snapshot(&self) -> GatewayDraftProgress {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        GatewayDraftProgress {
            private: state.private.clone(),
            status: state.status,
            step_label: state.step.clone(),
            message: state.message.clone(),
            warning: state.warning
                && (state.private.is_some() || state.status == GatewayDraftStatus::Attention),
            // Event delivery can lag task completion. Once started, submission may have
            // happened even if the handoff event has not reached this projection yet.
            can_retry: state.status == GatewayDraftStatus::Failed
                && !state.started
                && !state.handed_off,
        }
    }
}

const fn step_label(step: PublicActionProgressStep) -> &'static str {
    match step {
        PublicActionProgressStep::ShieldKey => "Authorize shield key",
        PublicActionProgressStep::Send => "Send",
        PublicActionProgressStep::Wrap => "Wrap native asset",
        PublicActionProgressStep::Approve => "Approve token",
        PublicActionProgressStep::Shield => "Shield",
        _ => "Transaction",
    }
}

#[cfg(test)]
mod tests;
