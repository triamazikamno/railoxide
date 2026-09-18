use super::*;
use gpui_component::{Sizable as _, WindowExt as _};
use tokio::sync::watch;
use wallet_ops::PublicActionProgressStatus;

use crate::root::{
    PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH,
    private_broadcaster::{
        PrivateBroadcasterProgressStepState, apply_private_broadcaster_progress_stage,
        private_broadcaster_progress_steps, private_broadcaster_stage_detail,
        private_broadcaster_stage_id,
    },
    public_action::PublicActionStepStatus,
    public_broadcaster_cost::render_public_broadcaster_tx_hash_row,
    secondary_dialog_content_width,
    submission_progress::{SubmissionProgressStep, render_submission_progress_stepper},
};

#[derive(Clone)]
pub(super) enum RecoveryProgressSource {
    Preparation,
    Native(watch::Receiver<Option<PublicActionProgressUpdate>>),
    Broadcaster(watch::Receiver<TransactionGenerationStage>),
    Retry(watch::Receiver<Option<PublicActionProgressUpdate>>),
}

impl RecoveryProgressSource {
    async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        match self {
            Self::Preparation => unreachable!("preparation has no stage updates"),
            Self::Native(receiver) | Self::Retry(receiver) => receiver.changed().await,
            Self::Broadcaster(receiver) => receiver.changed().await,
        }
    }
}

#[derive(Clone)]
struct RecoveryPreparationProgress {
    status: PublicActionStepStatus,
    message: String,
}

pub(super) struct RecoveryProgress {
    pub(super) open: bool,
    revision: u64,
    source: RecoveryProgressSource,
    preparation: Option<RecoveryPreparationProgress>,
    steps: Vec<PrivateBroadcasterProgressStepState>,
    tx_hash: Option<String>,
}

impl RecoveryProgress {
    fn new(revision: u64, source: RecoveryProgressSource, previous: Option<&Self>) -> Self {
        let preparation = if matches!(source, RecoveryProgressSource::Preparation) {
            Some(RecoveryPreparationProgress {
                status: PublicActionStepStatus::Pending,
                message: "Preparing recovery".into(),
            })
        } else if matches!(source, RecoveryProgressSource::Retry(_)) {
            None
        } else {
            previous
                .filter(|previous| matches!(previous.source, RecoveryProgressSource::Preparation))
                .and_then(|previous| previous.preparation.clone())
                .filter(|preparation| preparation.status == PublicActionStepStatus::Done)
        };
        let steps = match &source {
            RecoveryProgressSource::Preparation => Vec::new(),
            RecoveryProgressSource::Broadcaster(_) => private_broadcaster_progress_steps(),
            RecoveryProgressSource::Native(_) | RecoveryProgressSource::Retry(_) => [
                TransactionGenerationStage::SigningSelfBroadcast,
                TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, stage)| PrivateBroadcasterProgressStepState {
                stage,
                status: if index == 0 {
                    PublicActionStepStatus::Pending
                } else {
                    PublicActionStepStatus::NotStarted
                },
                message: None,
            })
            .collect(),
        };
        Self {
            open: false,
            revision,
            source,
            preparation,
            steps,
            tx_hash: None,
        }
    }

    fn update(&mut self) {
        match &self.source {
            RecoveryProgressSource::Preparation => {}
            RecoveryProgressSource::Broadcaster(receiver) => {
                apply_private_broadcaster_progress_stage(&mut self.steps, *receiver.borrow());
            }
            RecoveryProgressSource::Native(receiver) | RecoveryProgressSource::Retry(receiver) => {
                if let Some(update) = receiver.borrow().as_ref() {
                    if let Some(tx_hash) = &update.tx_hash {
                        self.tx_hash = Some(tx_hash.clone());
                    }
                    let stage = if self.tx_hash.is_some() {
                        TransactionGenerationStage::WaitingForSelfBroadcastReceipt
                    } else {
                        TransactionGenerationStage::SigningSelfBroadcast
                    };
                    apply_private_broadcaster_progress_stage(&mut self.steps, stage);
                    if let Some(step) = self.steps.iter_mut().find(|step| step.stage == stage) {
                        // Receipt observation alone is not recovery completion. The owner
                        // still has to validate the canonical recovery effects.
                        if update.status == PublicActionProgressStatus::Error {
                            step.status = PublicActionStepStatus::Error;
                        }
                        step.message = update.message.as_deref().map(Arc::from);
                    }
                }
            }
        }
    }

    pub(super) fn finish(&mut self, status: PublicActionStepStatus, message: String) {
        // Drain the latest watch value even if completion outruns the UI watcher.
        self.update();
        if matches!(self.source, RecoveryProgressSource::Preparation) {
            if let Some(preparation) = &mut self.preparation {
                preparation.status = status;
                preparation.message = message;
            }
            return;
        }
        if status == PublicActionStepStatus::Done {
            for step in &mut self.steps {
                step.status = PublicActionStepStatus::Done;
                step.message = None;
            }
        }
        let active = self
            .steps
            .iter()
            .position(|step| {
                matches!(
                    step.status,
                    PublicActionStepStatus::Pending | PublicActionStepStatus::Error
                )
            })
            .or_else(|| self.steps.len().checked_sub(1));
        if let Some(step) = active.and_then(|index| self.steps.get_mut(index)) {
            step.status = status;
            step.message = Some(message.into());
        }
    }

    pub(super) fn finish_broadcaster(&mut self, result: PublicBroadcasterResultKind) {
        let (message, status) = match result {
            PublicBroadcasterResultKind::Submitted { tx_hash } => {
                self.tx_hash = Some(tx_hash);
                ("Broadcaster submitted the transaction. Waiting for canonical recovery effects and private receipt.".to_owned(), PublicActionStepStatus::Done)
            }
            PublicBroadcasterResultKind::Failed { error } => {
                (format!("Broadcaster reported: {error}"), PublicActionStepStatus::Error)
            }
            PublicBroadcasterResultKind::TimedOut => {
                ("No broadcaster response yet. The issued payload remains tracked; inspect its status before retrying.".to_owned(), PublicActionStepStatus::Warning)
            }
        };
        self.finish(status, message);
    }

    fn render_content(&self) -> gpui::Div {
        let preparation = self
            .preparation
            .iter()
            .map(|preparation| SubmissionProgressStep {
                label: "Preparing recovery".into(),
                detail: preparation.message.clone(),
                status: preparation.status,
                error_copy_id: "stealth-recovery-preparation-error".into(),
                action: None,
            });
        let submission = self.steps.iter().map(|step| SubmissionProgressStep {
            label: match step.stage {
                TransactionGenerationStage::SigningSelfBroadcast => "Submitting recovery",
                TransactionGenerationStage::WaitingForSelfBroadcastReceipt => "Confirming recovery",
                stage => stage.label(),
            }
            .into(),
            detail: step
                .message
                .as_deref()
                .unwrap_or_else(|| match (step.stage, step.status) {
                    (
                        TransactionGenerationStage::SigningSelfBroadcast,
                        PublicActionStepStatus::Pending,
                    ) => "Signing and submitting the recovery transaction.",
                    (
                        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
                        PublicActionStepStatus::Pending,
                    ) => "Waiting for a receipt and checking recovery effects.",
                    _ => private_broadcaster_stage_detail(step.stage, step.status, false),
                })
                .into(),
            status: step.status,
            error_copy_id: format!(
                "stealth-recovery-{}-error",
                private_broadcaster_stage_id(step.stage)
            )
            .into(),
            action: None,
        });
        div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(render_submission_progress_stepper(
                preparation.chain(submission),
            ))
            .children(self.tx_hash.as_ref().map(|tx_hash| {
                render_public_broadcaster_tx_hash_row(
                    tx_hash.clone(),
                    format!("stealth-recovery-progress-copy-tx-{}", self.revision).into(),
                )
                .debug_selector(|| "stealth-recovery-progress-tx-hash".into())
            }))
    }
}

impl StealthAccountsView {
    pub(super) fn start_recovery_job<T: Send + 'static>(
        &mut self,
        source: RecoveryProgressSource,
        show_progress: bool,
        work: impl std::future::Future<Output = eyre::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Window, &mut Context<'_, Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        self.error = None;
        self.coverage = None;
        self.job_revision = self.job_revision.wrapping_add(1);
        let revision = self.job_revision;
        let join = self.runtime.spawn(work);
        self.job = Some(join.abort_handle());
        self.recovery.dialog = Some(RecoveryProgress::new(
            revision,
            source.clone(),
            self.recovery.dialog.as_ref(),
        ));
        if !matches!(source, RecoveryProgressSource::Preparation) {
            self.watch_recovery_progress(source, revision, cx);
        }
        if show_progress {
            self.show_recovery_progress(window, cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.job_revision != revision || !this.session_is_current(cx) {
                    return;
                }
                this.job = None;
                this.finish_recovery_progress();
                this.reload_records();
                this.refresh_visible(cx);
                match result {
                    Ok(Ok(value)) => apply(this, value, window, cx),
                    error => {
                        let message = match error {
                            Ok(Err(error)) => format!("{error:#}"),
                            _ => "The local operation stopped unexpectedly. Check the account before retrying.".into(),
                        };
                        if let Some(dialog) = &mut this.recovery.dialog {
                            dialog.finish(PublicActionStepStatus::Error, message.clone());
                        }
                        this.error = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn watch_recovery_progress(
        &mut self,
        mut source: RecoveryProgressSource,
        revision: u64,
        cx: &Context<'_, Self>,
    ) {
        self.recovery.progress = Some(cx.spawn(async move |this, cx| {
            while source.changed().await.is_ok() {
                let current = this.update(cx, |this, cx| {
                    if this.job_revision != revision
                        || this.job.is_none()
                        || !this.session_is_current(cx)
                    {
                        return false;
                    }
                    if let Some(progress) = &mut this.recovery.dialog {
                        progress.update();
                    }
                    cx.notify();
                    true
                });
                if !matches!(current, Ok(true)) {
                    break;
                }
            }
        }));
    }

    pub(super) fn show_recovery_progress(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = &mut self.recovery.dialog else {
            return;
        };
        if progress.open {
            return;
        }
        progress.open = true;
        let revision = progress.revision;
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            let width = (window.viewport_size().width * 0.92)
                .min(PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH);
            let content = view
                .update(cx, |view, cx| {
                    view.render_recovery_progress(cx)
                        .w(secondary_dialog_content_width(width))
                })
                .unwrap_or_else(|_| div());
            let close_view = view.clone();
            dialog
                .title(app_strong_text("Recover to private balance"))
                .w(width)
                .max_h(window.viewport_size().height * 0.84)
                .overlay_closable(false)
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| {
                    let _ = close_view.update(cx, |view, cx| {
                        if let Some(progress) = &mut view.recovery.dialog
                            && progress.revision == revision
                        {
                            progress.open = false;
                            cx.notify();
                        }
                    });
                })
                .child(content)
        });
    }

    pub(super) fn close_recovery_progress(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(progress) = &mut self.recovery.dialog
            && progress.open
        {
            progress.open = false;
            window.close_dialog(cx);
        }
    }

    fn render_recovery_progress(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let Some(progress) = &self.recovery.dialog else {
            return div();
        };
        if !self.session_is_current(cx) {
            return div().child(app_muted_text("Wallet session ended."));
        }
        let mut content = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "stealth-recovery-progress".into())
            .child(progress.render_content());
        if self.job.is_some() {
            let view = cx.entity();
            return content.child(
                ui::private_submission::operation_controls(
                    "stealth-recovery-progress",
                    [ui::private_submission::OperationControl::Stop],
                    move |_, _, cx| view.update(cx, Self::stop_work),
                )
                .w_full()
                .justify_end(),
            );
        }
        if self
            .recovery
            .prepared
            .as_ref()
            .is_some_and(|prepared| self.next_recovery_step(prepared).is_some())
        {
            content = content.child(
                app_button("stealth-recovery-progress-review", "Review recovery…")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_recovery_progress(window, cx);
                        this.request_recovery_submission(None, window, cx);
                    })),
            );
        }
        content.child(
            div().w_full().flex().justify_end().child(
                app_button("stealth-recovery-progress-close", "Close")
                    .small()
                    .flex_none()
                    .outline()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_recovery_progress(window, cx);
                        cx.notify();
                    })),
            ),
        )
    }

    pub(in crate::root::stealth_accounts) fn stop_work(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
        for observations in self.observations.values_mut() {
            observations.stop();
        }
        self.finish_recovery_progress();
        self.invalidate_recovery();
        self.job_revision = self.job_revision.wrapping_add(1);
        let message = "Local work stopped. Saved accounts and signed transactions remain. Incomplete checks are unknown.";
        self.coverage = Some(message.into());
        if let Some(progress) = &mut self.recovery.dialog {
            progress.finish(PublicActionStepStatus::Stopped, message.into());
        }
        self.reload_records();
        self.refresh_visible(cx);
        cx.notify();
    }
}

pub(super) const fn recovery_execution_status(
    status: ExecutorPayloadStatus,
) -> PublicActionStepStatus {
    match status {
        ExecutorPayloadStatus::Executed => PublicActionStepStatus::Done,
        ExecutorPayloadStatus::Uncertain => PublicActionStepStatus::Warning,
        ExecutorPayloadStatus::Reverted
        | ExecutorPayloadStatus::MissingEffects
        | ExecutorPayloadStatus::Invalidated { .. } => PublicActionStepStatus::Error,
    }
}

#[cfg(test)]
mod tests;
