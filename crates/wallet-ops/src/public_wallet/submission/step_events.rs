#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::public_wallet) fn emit_public_action_event(
    event_tx: Option<&PublicActionSessionEventSender>,
    event: PublicActionSessionEvent,
) {
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(event);
    }
}

pub(in crate::public_wallet) fn emit_refreshed_public_action_hardware_session(
    event_tx: Option<&PublicActionSessionEventSender>,
    signer: &VaultedPublicSigner,
) {
    match signer.refreshed_hardware_session() {
        Ok(Some(session)) => emit_public_action_event(
            event_tx,
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { session },
        ),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(target: LOG_TARGET,%error, "failed to read refreshed hardware public signer session");
        }
    }
}

pub(in crate::public_wallet) async fn recv_public_action_command(
    command_rx: &mut Option<PublicActionCommandReceiver>,
) -> Option<PublicActionCommand> {
    let command_rx = command_rx.as_mut()?;
    command_rx.recv().await
}

#[must_use]
pub const fn public_action_replacement_bumped_fee(value: u128) -> u128 {
    self_broadcast_replacement_bumped_fee(value)
}

pub(in crate::public_wallet) const fn public_action_progress_update(
    step: PublicActionProgressStep,
    status: PublicActionProgressStatus,
    tx_hash: Option<String>,
    message: Option<String>,
) -> PublicActionProgressUpdate {
    PublicActionProgressUpdate {
        step,
        status,
        tx_hash,
        message,
    }
}
