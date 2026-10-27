//! One stealth account's setup as the stages its broadcaster request goes through: the swap's
//! own account's, or a private Bridge swap's destination account's.
//!
//! The value outlives the job that sent the setup, so the stage it ended at stays known until
//! a new attempt for that account starts or the swap leaves its setup.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::Task;
use tokio::sync::watch;
use wallet_ops::{
    PublicBroadcasterResultKind, SubmissionRouteProgress, SubmissionRouteState,
    TransactionGenerationStage,
};

use super::form::broadcaster_result_problem;
use super::{SWAP_BROADCASTER_RESPONSE_TIMEOUT, SWAP_SETUP_RETRY_RESENDS};
use crate::root::private_broadcaster::{
    PrivateBroadcasterProgressStepState, apply_private_broadcaster_progress_stage,
    broadcaster_wait_detail, end_private_broadcaster_progress_steps,
    private_broadcaster_progress_steps,
};
use crate::root::public_action::PublicActionStepStatus;
use crate::root::submission_route::SubmissionRouteCard;

/// What the response stage says after a broadcaster didn't answer in time.
const SETUP_TIMED_OUT: &str = "No broadcaster response. The setup can still be included, and the swap continues if it is. Retry setup… sends it from a new identity.";

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
    /// The route watch of the running job's submission. An ended setup has none.
    route: Option<watch::Receiver<SubmissionRouteProgress>>,
    end: Option<SetupProgressEnd>,
    /// Redraws the view on each stage and route change. A value built without a view has none.
    watch: Option<Task<()>>,
    /// When the setup began to wait for a broadcaster's answer, as its watch saw it.
    waiting_since: Rc<Cell<Option<Instant>>>,
}

impl SetupProgress {
    /// A setup whose job reports its stages through `receiver`, and its submission's route
    /// through `route`.
    pub(super) fn running(
        receiver: watch::Receiver<TransactionGenerationStage>,
        route: watch::Receiver<SubmissionRouteProgress>,
        watch: Option<Task<()>>,
    ) -> Self {
        Self {
            steps: private_broadcaster_progress_steps(),
            receiver: Some(receiver),
            route: Some(route),
            end: None,
            watch,
            waiting_since: Rc::default(),
        }
    }

    /// The setup with `waiting_since`, which its watch sets once the setup waits for a
    /// broadcaster's answer.
    pub(super) fn waiting_since(mut self, waiting_since: Rc<Cell<Option<Instant>>>) -> Self {
        self.waiting_since = waiting_since;
        self
    }

    /// Whether the setup still waits for a broadcaster's answer after the re-sends that offer
    /// to send it again, `SWAP_SETUP_RETRY_RESENDS`.
    pub(super) fn wait_outlasted(&self) -> bool {
        self.stage() == Some(TransactionGenerationStage::WaitingForBroadcasterResponse)
            && self.resends() >= SWAP_SETUP_RETRY_RESENDS
    }

    /// How often the running job re-sent the setup: each publish its route reports after the
    /// first.
    fn resends(&self) -> u32 {
        self.route()
            .map_or(0, |route| route.publishes.saturating_sub(1))
    }

    /// Where the running job's route stands. `None` once the job ended.
    pub(super) fn route(&self) -> Option<SubmissionRouteProgress> {
        self.route.as_ref().map(|route| *route.borrow())
    }

    /// What `step` says while the setup waits for a broadcaster's answer: how often the
    /// request was re-sent and the time left, as a private action's progress says it. `None`
    /// for any other step, and once the setup's job ended.
    pub(super) fn wait_detail(
        &self,
        step: &PrivateBroadcasterProgressStepState,
        now: Instant,
    ) -> Option<String> {
        if step.stage != TransactionGenerationStage::WaitingForBroadcasterResponse
            || step.status != PublicActionStepStatus::Pending
            || self.stage()? != step.stage
        {
            return None;
        }
        Some(broadcaster_wait_detail(
            self.resends(),
            self.waiting_since.get()?,
            Some(SWAP_BROADCASTER_RESPONSE_TIMEOUT),
            now,
        ))
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
        self.route = None;
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

/// The route card of a swap's setups, each with the network its heading names: the routes of
/// the setups whose job still runs and whose route isn't closed. `None` while there are none,
/// which shows no chip.
pub(super) fn setup_route_card<'a>(
    isolated: bool,
    setups: impl IntoIterator<Item = (Option<String>, &'a SetupProgress)>,
) -> Option<SubmissionRouteCard> {
    let routes: Vec<_> = setups
        .into_iter()
        .filter_map(|(network, setup)| Some((network, setup.route()?)))
        .filter(|(_, route)| route.state != SubmissionRouteState::Closed)
        .collect();
    (!routes.is_empty()).then_some(SubmissionRouteCard {
        isolated,
        ended: false,
        routes,
    })
}

#[cfg(test)]
mod tests {
    use super::PublicActionStepStatus::{Done, Error, NotStarted, Pending};
    use super::*;

    /// A running setup at `stage` whose route stands at `route`, with the senders of both.
    fn running(
        stage: TransactionGenerationStage,
        route: SubmissionRouteProgress,
    ) -> (
        SetupProgress,
        watch::Sender<TransactionGenerationStage>,
        watch::Sender<SubmissionRouteProgress>,
    ) {
        let (stages, receiver) = watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        stages.send(stage).unwrap();
        let (routes, route_receiver) = watch::channel(route);
        (
            SetupProgress::running(receiver, route_receiver, None),
            stages,
            routes,
        )
    }

    const fn route(state: SubmissionRouteState, peers: usize) -> SubmissionRouteProgress {
        SubmissionRouteProgress {
            state,
            peers,
            recoveries_used: 0,
            publishes: 0,
        }
    }

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
        let (mut progress, _stages, _routes) = running(stage, SubmissionRouteProgress::default());
        assert!(progress.finish(result));
        assert_eq!(progress.stage(), None);
        progress
    }

    /// While a broadcaster is awaited, its stage tells the re-sends its route made and the
    /// time left, as a private action's progress does, with the swap's own timeout.
    #[test]
    fn the_response_stage_tells_the_resends_while_waiting() {
        let (progress, sender, routes) = running(
            TransactionGenerationStage::WaitingForBroadcasterResponse,
            SubmissionRouteProgress::default(),
        );
        let now = Instant::now();
        let waiting_since = Rc::new(Cell::new(None));
        let progress = progress.waiting_since(Rc::clone(&waiting_since));
        // 12 s in, two republish intervals passed, and a reconnect let one re-send through.
        waiting_since.set(now.checked_sub(std::time::Duration::from_secs(12)));
        routes.send_modify(|route| route.publishes = 2);
        let details = |progress: &SetupProgress| {
            progress
                .steps()
                .iter()
                .filter_map(|step| progress.wait_detail(step, now))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            details(&progress),
            ["Still waiting - re-sent 1x - 1:48 left"]
        );
        sender
            .send(TransactionGenerationStage::PublishingToBroadcaster)
            .unwrap();
        assert_eq!(details(&progress), [] as [String; 0]);
    }

    /// Retry setup… waits for five re-sends the route made: a route that reconnected through
    /// more than five republish intervals re-sent fewer.
    #[test]
    fn the_retry_waits_for_five_actual_resends() {
        let (progress, _stages, routes) = running(
            TransactionGenerationStage::WaitingForBroadcasterResponse,
            SubmissionRouteProgress::default(),
        );
        let progress = progress.waiting_since(Rc::new(Cell::new(
            Instant::now().checked_sub(std::time::Duration::from_secs(40)),
        )));
        routes.send_modify(|route| route.publishes = 5);
        assert!(!progress.wait_outlasted());
        routes.send_modify(|route| route.publishes = 6);
        assert!(progress.wait_outlasted());
    }

    /// One chip covers a swap's running setups: the least ready route, with the peers of
    /// both. A closed route drops out before its setup's job ends, as does a setup whose job
    /// ended, and with none left there is no chip.
    #[test]
    fn running_setups_show_one_route() {
        use SubmissionRouteState::{Closed, Ready, Reconnecting};
        let waiting = TransactionGenerationStage::WaitingForBroadcasterResponse;
        let (origin, _origin_stages, origin_routes) = running(waiting, route(Ready, 3));
        let (mut destination, _destination_stages, destination_routes) =
            running(waiting, route(Ready, 2));
        let card = |origin: &SetupProgress, destination: &SetupProgress| {
            setup_route_card(
                true,
                [
                    (Some("Ethereum".to_owned()), origin),
                    (Some("Arbitrum One".to_owned()), destination),
                ],
            )
        };

        let both = card(&origin, &destination).unwrap();
        assert_eq!(both.routes.len(), 2);
        let indicator = both.indicator();
        assert_eq!(
            (indicator.label, indicator.peers),
            ("Isolated route", Some(5))
        );

        origin_routes.send_modify(|route| route.state = Reconnecting);
        let indicator = card(&origin, &destination).unwrap().indicator();
        assert_eq!((indicator.label, indicator.peers), ("Reconnecting", None));

        // The reconnecting side's submission stopped: the ready side alone sets the chip.
        origin_routes.send_modify(|route| route.state = Closed);
        let left = card(&origin, &destination).unwrap();
        assert_eq!(
            left.routes,
            [(Some("Arbitrum One".to_owned()), route(Ready, 2))]
        );
        let indicator = left.indicator();
        assert_eq!(
            (indicator.label, indicator.peers),
            ("Isolated route", Some(2))
        );

        destination_routes.send_modify(|route| route.state = Closed);
        assert_eq!(card(&origin, &destination), None);

        // An ended setup drops out whatever its route last read.
        destination_routes.send_modify(|route| route.state = Ready);
        assert!(destination.finish(Ok(&PublicBroadcasterResultKind::TimedOut)));
        assert_eq!(card(&origin, &destination), None);
    }

    #[test]
    fn stages_sent_in_order_complete_the_earlier_steps() {
        let (progress, sender, _routes) = running(
            TransactionGenerationStage::SelectingPrivateNotes,
            SubmissionRouteProgress::default(),
        );
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
        let (_routes, route) = watch::channel(SubmissionRouteProgress::default());
        let mut progress = SetupProgress::running(receiver, route, None);
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
