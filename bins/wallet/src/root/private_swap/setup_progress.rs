//! One stealth account's setup as the stages its broadcaster request goes through: the swap's
//! own account's, or a private Bridge swap's destination account's.
//!
//! The value outlives the job that sent the setup, so the stage it ended at stays known until
//! a new attempt for that account starts or the swap leaves its setup.

use std::sync::Arc;

use gpui::Task;
use tokio::sync::watch;
use wallet_ops::{PublicBroadcasterResultKind, TransactionGenerationStage};

use super::form::broadcaster_result_problem;
use crate::root::private_broadcaster::{
    PrivateBroadcasterProgressStepState, apply_private_broadcaster_progress_stage,
    end_private_broadcaster_progress_steps, private_broadcaster_progress_steps,
};
use crate::root::public_action::PublicActionStepStatus;

/// What the response stage says after a broadcaster didn't answer in time.
const SETUP_TIMED_OUT: &str =
    "No broadcaster response. The setup can still be included, and the swap continues if it is.";

/// How the job that sent a setup ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupProgressEnd {
    /// The job failed without a broadcaster's answer.
    Failed,
    /// The broadcaster reported a problem.
    BroadcasterFailed,
    /// No broadcaster answered in time. The setup can still be included.
    TimedOut,
    /// The broadcaster submitted the setup.
    Submitted,
}

/// The stages of one side's setup in this session.
pub(super) struct SetupProgress {
    steps: Vec<PrivateBroadcasterProgressStepState>,
    /// The stage watch of the running job. An ended setup has none.
    receiver: Option<watch::Receiver<TransactionGenerationStage>>,
    end: Option<SetupProgressEnd>,
    /// Redraws the view on each stage. A value built without a view has none.
    watch: Option<Task<()>>,
}

impl SetupProgress {
    /// A setup whose job reports its stages through `receiver`.
    pub(super) fn running(
        receiver: watch::Receiver<TransactionGenerationStage>,
        watch: Option<Task<()>>,
    ) -> Self {
        Self {
            steps: private_broadcaster_progress_steps(),
            receiver: Some(receiver),
            end: None,
            watch,
        }
    }

    /// The stage a running setup is at. `None` once its job ended.
    pub(super) fn stage(&self) -> Option<TransactionGenerationStage> {
        self.receiver.as_ref().map(|receiver| *receiver.borrow())
    }

    /// How the setup's job ended. `None` while it runs.
    pub(super) const fn end(&self) -> Option<SetupProgressEnd> {
        self.end
    }

    /// The stages as they stand: a running setup's follow its watch, and an ended one's stay
    /// as its job left them.
    pub(super) fn steps(&self) -> Vec<PrivateBroadcasterProgressStepState> {
        let mut steps = self.steps.clone();
        if let Some(stage) = self.stage() {
            apply_private_broadcaster_progress_stage(&mut steps, stage);
        }
        steps
    }

    /// What the stage an ended setup stopped at says of its problem. `None` while the setup
    /// runs and once a broadcaster submitted it.
    pub(super) fn problem(&self) -> Option<Arc<str>> {
        self.end?;
        self.steps.iter().find_map(|step| step.message.clone())
    }

    /// End the stages with the job's result: a broadcaster's answer, or the error the job
    /// failed with before one. A setup ends once: a later result changes nothing, and `false`
    /// tells that these stages didn't take it.
    pub(super) fn finish(&mut self, result: Result<&PublicBroadcasterResultKind, String>) -> bool {
        if self.end.is_some() {
            return false;
        }
        // A job that failed before its setup reported a stage failed at none of them.
        if result.is_err()
            && self
                .receiver
                .as_ref()
                .is_some_and(|receiver| !receiver.borrow().has_changed())
        {
            return false;
        }
        // Drain the latest watch value even if completion outruns the UI watcher.
        if let Some(receiver) = self.receiver.take() {
            apply_private_broadcaster_progress_stage(&mut self.steps, *receiver.borrow());
        }
        self.watch = None;
        let (end, message) = match result {
            Ok(result) => match result {
                PublicBroadcasterResultKind::Submitted { .. } => {
                    (SetupProgressEnd::Submitted, None)
                }
                PublicBroadcasterResultKind::Failed { .. } => (
                    SetupProgressEnd::BroadcasterFailed,
                    broadcaster_result_problem(result, "setup"),
                ),
                PublicBroadcasterResultKind::TimedOut => {
                    (SetupProgressEnd::TimedOut, Some(SETUP_TIMED_OUT.to_owned()))
                }
            },
            Err(error) => (SetupProgressEnd::Failed, Some(error)),
        };
        let status = match end {
            SetupProgressEnd::Submitted => PublicActionStepStatus::Done,
            // A broadcaster answers, or fails to, only after the request was published.
            SetupProgressEnd::BroadcasterFailed | SetupProgressEnd::TimedOut => {
                apply_private_broadcaster_progress_stage(
                    &mut self.steps,
                    TransactionGenerationStage::WaitingForBroadcasterResponse,
                );
                PublicActionStepStatus::Error
            }
            SetupProgressEnd::Failed => PublicActionStepStatus::Error,
        };
        end_private_broadcaster_progress_steps(
            &mut self.steps,
            status,
            message.map(Arc::<str>::from),
        );
        self.end = Some(end);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::PublicActionStepStatus::{Done, Error, NotStarted, Pending};
    use super::*;

    fn statuses(progress: &SetupProgress) -> Vec<PublicActionStepStatus> {
        progress.steps().iter().map(|step| step.status).collect()
    }

    fn messages(progress: &SetupProgress) -> Vec<Option<Arc<str>>> {
        progress
            .steps()
            .into_iter()
            .map(|step| step.message)
            .collect()
    }

    /// A setup whose job ended with `result` while its watch was at `stage`.
    fn ended(
        stage: TransactionGenerationStage,
        result: Result<&PublicBroadcasterResultKind, String>,
    ) -> SetupProgress {
        let (sender, receiver) = watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let mut progress = SetupProgress::running(receiver, None);
        sender.send(stage).unwrap();
        assert!(progress.finish(result));
        assert_eq!(progress.stage(), None);
        progress
    }

    #[test]
    fn stages_sent_in_order_complete_the_earlier_steps() {
        let (sender, receiver) = watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let progress = SetupProgress::running(receiver, None);
        for (index, step) in private_broadcaster_progress_steps().iter().enumerate() {
            sender.send(step.stage).unwrap();
            let statuses = statuses(&progress);
            assert!(statuses[..index].iter().all(|status| *status == Done));
            assert_eq!(statuses[index], Pending);
            assert!(
                statuses[index + 1..]
                    .iter()
                    .all(|status| *status == NotStarted)
            );
            assert_eq!(progress.stage(), Some(step.stage));
        }
    }

    /// Preparation can fail before the setup reports its first stage. That failure belongs to
    /// no stage, so the stages don't take it and the swap's own error tells it.
    #[test]
    fn a_failure_before_the_first_stage_is_no_stages_failure() {
        let (_sender, receiver) = watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let mut progress = SetupProgress::running(receiver, None);
        assert!(!progress.finish(Err("The account can't be used.".to_owned())));
        assert_eq!(progress.end(), None);
        assert_eq!(progress.problem(), None);
    }

    #[test]
    fn a_job_result_ends_the_steps_by_its_kind() {
        // An error before publishing stays on the step that was running.
        let failed = ended(
            TransactionGenerationStage::ProvingTransaction,
            Err("The proof could not be generated.".to_owned()),
        );
        assert_eq!(failed.end, Some(SetupProgressEnd::Failed));
        assert_eq!(
            statuses(&failed),
            [Done, Error, NotStarted, NotStarted, NotStarted, NotStarted]
        );
        assert_eq!(
            messages(&failed)[1].as_deref(),
            Some("The proof could not be generated.")
        );

        // A broadcaster's answer is about the response, wherever the watch was read last.
        let refused = ended(
            TransactionGenerationStage::PublishingToBroadcaster,
            Ok(&PublicBroadcasterResultKind::Failed {
                error: "fee too low".to_owned(),
            }),
        );
        assert_eq!(refused.end, Some(SetupProgressEnd::BroadcasterFailed));
        assert_eq!(statuses(&refused), [Done, Done, Done, Done, Done, Error]);
        assert_eq!(
            messages(&refused)[5].as_deref(),
            Some("The broadcaster reported a problem with the setup: fee too low")
        );

        let timed_out = ended(
            TransactionGenerationStage::WaitingForBroadcasterResponse,
            Ok(&PublicBroadcasterResultKind::TimedOut),
        );
        assert_eq!(timed_out.end, Some(SetupProgressEnd::TimedOut));
        assert_eq!(statuses(&timed_out), [Done, Done, Done, Done, Done, Error]);
        assert_eq!(messages(&timed_out)[5].as_deref(), Some(SETUP_TIMED_OUT));
        assert_eq!(timed_out.problem().as_deref(), Some(SETUP_TIMED_OUT));

        let submitted = ended(
            TransactionGenerationStage::WaitingForBroadcasterResponse,
            Ok(&PublicBroadcasterResultKind::Submitted {
                tx_hash: "0x01".to_owned(),
            }),
        );
        assert_eq!(submitted.end, Some(SetupProgressEnd::Submitted));
        assert_eq!(statuses(&submitted), [Done; 6]);
        assert!(messages(&submitted).iter().all(Option::is_none));
        assert_eq!(submitted.problem(), None);
    }
}
