//! Public draft messages and transaction progress shared with the desktop owner.
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayDraftInput {
    pub account: String,
    pub chain_id: u64,
    pub kind: GatewayDraftKind,
    pub asset: String,
    pub amount: String,
    pub recipient: String,
    pub address_book_entry: Option<String>,
    pub fee: GatewayDraftFee,
    pub mimic_railway: bool,
    pub max: bool,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayDraftCommand {
    Create {
        request_id: String,
        input: GatewayDraftInput,
    },
    Update {
        draft_id: String,
        revision: u64,
        input: GatewayDraftInput,
    },
    Submit {
        draft_id: String,
        revision: u64,
    },
    Cancel {
        draft_id: String,
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
    pub input: GatewayDraftInput,
    pub status: GatewayDraftStatus,
    pub estimate: Option<GatewayDraftEstimate>,
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
}

#[derive(Clone, PartialEq, Eq)]
pub struct GatewayDraftProgress {
    pub status: GatewayDraftStatus,
    pub step_label: String,
    pub message: String,
    pub warning: bool,
    pub can_retry: bool,
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
        })))
    }
}

impl GatewayDraftExecution {
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
        if state.started || state.review_approved {
            return;
        }
        state.cancelled = true;
        state.status = GatewayDraftStatus::Failed;
        state.step = "Canceled".into();
        state.message = "The transaction was not submitted.".into();
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
            status: state.status,
            step_label: state.step.clone(),
            message: state.message.clone(),
            warning: state.status == GatewayDraftStatus::Attention && state.warning,
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
mod tests {
    use super::*;

    #[test]
    fn canceled_reviews_and_duplicate_approvals_cannot_start_another_transaction() {
        let canceled = GatewayDraftExecution::default();
        let late_approval = canceled.clone();
        canceled.cancel_review();
        assert!(!late_approval.approve_review());
        assert!(!late_approval.start());
        assert!(canceled.snapshot().can_retry);

        let execution = GatewayDraftExecution::default();
        let duplicate = execution.clone();
        assert!(!execution.start());
        assert!(execution.approve_review());
        execution.cancel_review(); // Closing the successfully approved dialog is not rejection.
        assert!(execution.start());
        assert!(!duplicate.start());
        execution.finish(false); // The task can finish before queued handoff events are delivered.
        assert!(!execution.snapshot().can_retry);
        duplicate.event(&PublicActionSessionEvent::AttemptHandoff {
            step: PublicActionProgressStep::Send,
        });
        assert_eq!(execution.snapshot().status, GatewayDraftStatus::Failed);
        assert!(!execution.snapshot().can_retry);
    }

    #[test]
    fn recurring_shield_attention_survives_observer_changes_and_terminal_state_is_stable() {
        let owner = GatewayDraftExecution::default();
        assert!(!owner.snapshot().warning);
        assert!(owner.approve_review());
        assert!(owner.start());
        owner.event(&PublicActionSessionEvent::HardwareApprovalStarted);
        assert_eq!(owner.snapshot().status, GatewayDraftStatus::Attention);
        owner.event(&PublicActionSessionEvent::HardwareApprovalFailed {
            message: "private diagnostic omitted".into(),
        });
        assert!(owner.snapshot().warning);
        owner.event(&PublicActionSessionEvent::HardwareApprovalStarted);
        assert!(!owner.snapshot().warning);
        owner.event(&PublicActionSessionEvent::HardwareApprovalCompleted);
        assert_eq!(owner.snapshot().status, GatewayDraftStatus::InProgress);
        owner.event(&PublicActionSessionEvent::AttemptHandoff {
            step: PublicActionProgressStep::Approve,
        });
        let reopened = owner.clone();
        owner.event(&PublicActionSessionEvent::StepFailed {
            step: PublicActionProgressStep::Shield,
            message: "private diagnostic omitted".into(),
        });
        assert!(reopened.snapshot().warning);
        owner.event(&PublicActionSessionEvent::FeeAuthorizationRequired {
            step: PublicActionProgressStep::Shield,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 1,
            message: "private diagnostic omitted".into(),
        });
        // Progress and session events use separate channels; old progress may arrive late.
        owner.progress(&PublicActionProgressUpdate {
            step: PublicActionProgressStep::Approve,
            status: PublicActionProgressStatus::Pending,
            tx_hash: None,
            message: None,
        });
        assert_eq!(reopened.snapshot().status, GatewayDraftStatus::Attention);
        assert!(!reopened.snapshot().warning);
        assert_eq!(reopened.snapshot().step_label, "Shield");
        assert!(!reopened.snapshot().message.contains("private diagnostic"));
        owner.finish(true);
        owner.event(&PublicActionSessionEvent::HardwareApprovalFailed {
            message: "late".into(),
        });
        assert_eq!(reopened.snapshot().status, GatewayDraftStatus::Done);
        assert!(!reopened.snapshot().warning);
        assert!(!reopened.snapshot().can_retry);
    }
}
