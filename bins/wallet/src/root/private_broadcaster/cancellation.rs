#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) fn cancel_private_broadcaster_progress(
    progress: &mut Option<PrivateBroadcasterProgressState>,
) -> Option<tokio::task::AbortHandle> {
    let mut progress = progress.take()?;
    if let Some(command_tx) = progress.sponsored_self_broadcast_command_tx.take() {
        let _ = command_tx.send(SponsoredSelfBroadcastCommand::Shutdown);
        let session_active = progress.steps.iter().any(|step| {
            step.status == PublicActionStepStatus::Pending
                && sponsored_stop_uses_session_command(step.stage)
        });
        let handle = progress.task_abort_handle.take();
        if !session_active {
            if let Some(handle) = handle {
                handle.abort();
            }
            return None;
        }
        return handle;
    }
    if let Some(handle) = progress.task_abort_handle.take() {
        handle.abort();
    }
    None
}

pub(in crate::root) fn request_sponsored_session_cancel(
    progress: &mut PrivateBroadcasterProgressState,
) -> bool {
    if let Some(command_tx) = progress.sponsored_self_broadcast_command_tx.as_ref() {
        let _ = command_tx.send(SponsoredSelfBroadcastCommand::Cancel);
        true
    } else if let Some(handle) = progress.task_abort_handle.take() {
        handle.abort();
        false
    } else {
        false
    }
}

pub(in crate::root) fn cancel_public_broadcaster_tasks(
    handles: &mut Vec<tokio::task::AbortHandle>,
) {
    for handle in handles.drain(..) {
        handle.abort();
    }
}

pub(in crate::root) fn spawn_sponsored_abort_watchdog(
    runtime: &tokio::runtime::Handle,
    handle: tokio::task::AbortHandle,
    grace: Duration,
) {
    drop(runtime.spawn(async move {
        tokio::time::sleep(grace).await;
        if !handle.is_finished() {
            handle.abort();
        }
    }));
}
