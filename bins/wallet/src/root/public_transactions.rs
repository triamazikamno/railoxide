use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use gpui::Context;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};
use wallet_ops::{PublicTransactionTracker, PublicTransactionTrackingContext};

use super::WalletRoot;

/// The root retains the actual submission jobs even when their UI completion task disappears.
#[derive(Default)]
pub(super) struct PublicTransactionSubmissions {
    closed: bool,
    tasks: Vec<JoinHandle<()>>,
}

pub(super) struct PublicTransactionSubmission<T> {
    result: oneshot::Receiver<T>,
    abort: Option<AbortHandle>,
}

impl<T> PublicTransactionSubmission<T> {
    pub(super) fn abort_handle(&self) -> Option<AbortHandle> {
        self.abort.clone()
    }
}

impl<T> Future for PublicTransactionSubmission<T> {
    type Output = Result<T, oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.result).poll(cx)
    }
}

#[derive(Clone)]
pub(super) struct PublicTransactionCleanup {
    completed: watch::Receiver<bool>,
}

impl PublicTransactionCleanup {
    pub(super) fn is_finished(&self) -> bool {
        *self.completed.borrow()
    }

    pub(super) async fn wait(mut self) -> Result<(), String> {
        while !*self.completed.borrow_and_update() {
            self.completed
                .changed()
                .await
                .map_err(|_| "Public transaction cleanup ended before completion".to_owned())?;
        }
        Ok(())
    }
}

impl PublicTransactionSubmissions {
    fn spawn<T: Send + 'static>(
        &mut self,
        runtime: &Handle,
        future: impl Future<Output = T> + Send + 'static,
    ) -> PublicTransactionSubmission<T> {
        let (sender, result) = oneshot::channel();
        if self.closed {
            return PublicTransactionSubmission {
                result,
                abort: None,
            };
        }
        let task = runtime.spawn(async move {
            let _ = sender.send(future.await);
        });
        let abort = Some(task.abort_handle());
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(task);
        PublicTransactionSubmission { result, abort }
    }

    fn shutdown(
        &mut self,
        runtime: &Handle,
        tracker: PublicTransactionTracker,
    ) -> PublicTransactionCleanup {
        self.closed = true;
        tracker.close();
        let tasks = std::mem::take(&mut self.tasks);
        for task in &tasks {
            task.abort();
        }
        let (completed, receiver) = watch::channel(false);
        runtime.spawn(async move {
            for task in tasks {
                let _ = task.await;
            }
            tracker.shutdown().await;
            let _ = completed.send(true);
        });
        PublicTransactionCleanup {
            completed: receiver,
        }
    }
}

impl WalletRoot {
    pub(super) fn public_transaction_tracking_context(
        &self,
        chain_id: u64,
        account_uuid: &str,
    ) -> Result<PublicTransactionTrackingContext, String> {
        if self.public_transaction_submissions.closed {
            return Err("Public transaction cleanup is in progress".to_owned());
        }
        let scope = self.current_public_balance_scope(chain_id).ok_or_else(|| {
            "Unlock the current wallet and finish maintenance before submitting transactions"
                .to_owned()
        })?;
        let account = self
            .public_account_for_uuid(Some(account_uuid))
            .filter(|account| {
                self.view_session
                    .as_ref()
                    .is_some_and(|view| account.is_scoped_to_wallet(view.wallet_id()))
            })
            .ok_or_else(|| "The public transaction account is no longer available".to_owned())?;
        self.publish_gateway_desktop_state();
        Ok(self.public_transaction_tracker.context(
            scope,
            account.clone(),
            self.public_balance_cache.clone(),
        ))
    }

    pub(super) fn spawn_public_transaction_submission<T: Send + 'static>(
        &mut self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> PublicTransactionSubmission<T> {
        self.public_transaction_submissions
            .spawn(&self.runtime, future)
    }

    pub(super) fn begin_public_transaction_shutdown(&mut self) -> PublicTransactionCleanup {
        if let Some(cleanup) = self.public_transaction_cleanup.as_ref() {
            return cleanup.clone();
        }
        let cleanup = self
            .public_transaction_submissions
            .shutdown(&self.runtime, self.public_transaction_tracker.clone());
        self.public_transaction_cleanup = Some(cleanup.clone());
        cleanup
    }

    pub(super) fn resume_public_transactions(&mut self) {
        if self
            .public_transaction_cleanup
            .as_ref()
            .is_some_and(PublicTransactionCleanup::is_finished)
        {
            self.public_transaction_tracker = self.public_transaction_tracker.successor();
            self.public_transaction_submissions = PublicTransactionSubmissions::default();
            self.public_transaction_cleanup = None;
            self.publish_gateway_desktop_state();
        }
    }

    pub(super) fn watch_public_transactions(&self, cx: &Context<'_, Self>) {
        let mut changes = self.public_transaction_tracker.subscribe();
        let mut shutdown = self.root_shutdown.subscribe();
        cx.spawn(async move |this, cx| {
            loop {
                tokio::select! {
                    result = changes.changed() => if result.is_err() { break; },
                    _ = shutdown.changed() => break,
                }
                if this
                    .update(cx, |root, cx| {
                        root.publish_gateway_desktop_state();
                        if !root.public_transaction_submissions.closed {
                            for ticket in root.public_balance_cache.take_queued_refreshes() {
                                root.run_public_balance_refresh(ticket, None, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn destructive_cleanup_awaits_submission_retirement_and_closes_admission() {
        struct DelayedRetirement {
            entered: Option<oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for DelayedRetirement {
            fn drop(&mut self) {
                let _ = self.entered.take().expect("retirement signal").send(());
                let _ = self.release.recv();
            }
        }

        let runtime = Handle::current();
        let tracker = PublicTransactionTracker::default();
        let mut submissions = PublicTransactionSubmissions::default();
        let (started_tx, started_rx) = oneshot::channel();
        let (retiring_tx, retiring_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let retirement = DelayedRetirement {
            entered: Some(retiring_tx),
            release: release_rx,
        };
        let submission = submissions.spawn(&runtime, async move {
            let _retirement = retirement;
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("submission started");
        let cleanup = submissions.shutdown(&runtime, tracker);
        retiring_rx.await.expect("submission is retiring");
        assert!(!cleanup.is_finished());
        let rejected: PublicTransactionSubmission<()> =
            submissions.spawn(&runtime, async { panic!("closed admission polled work") });
        assert!(rejected.await.is_err());
        release_tx.send(()).expect("release retirement");
        cleanup.wait().await.expect("cleanup completed");
        assert!(submission.await.is_err());
    }
}
