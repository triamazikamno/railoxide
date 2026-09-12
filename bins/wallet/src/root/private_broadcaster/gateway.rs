//! Browser progress follows the native operation, including after its dialog is hidden.
use super::*;
use wallet_ops::gateway::{
    GatewayDraftExecution, GatewayDraftStatus, GatewayPrivateDisplayRow,
    GatewayPrivateDraftControl, GatewayPrivateDraftResult,
};

impl WalletRoot {
    pub(in crate::root) fn publish_gateway_private_progress(&self) {
        let Some(progress) = &self.private_broadcaster_progress else {
            return;
        };
        let Some(execution) = &progress.gateway_execution else {
            return;
        };
        let Some(mut projection) = execution.snapshot().private else {
            return;
        };
        let terminal = private_broadcaster_progress_is_terminal(progress);
        projection.stop = !terminal && progress.stop_available;
        projection.stop_waiting = !terminal
            && !progress.stop_available
            && public_broadcaster_waiting_can_stop(progress, Instant::now());
        let address = public_broadcaster_progress_address(progress);
        projection.ban = projection.stop_waiting
            && address.is_some_and(|address| !self.is_banned_broadcaster(address));
        projection.context = [
            ("Asset", progress.asset_label.as_ref()),
            ("Recipient", progress.recipient.as_ref()),
        ]
        .into_iter()
        .map(|(label, value)| {
            let mut row = GatewayPrivateDisplayRow::default();
            row.label = label.into();
            row.value = value.into();
            row
        })
        .collect();
        if let Some(context) = self.private_broadcaster_progress_context(progress) {
            projection.context =
                crate::root::public_broadcaster_cost::private_broadcaster_progress_context_rows(
                    progress, &context,
                )
                .into_iter()
                .map(|row| {
                    let mut wire = GatewayPrivateDisplayRow::default();
                    wire.label = row.label;
                    wire.value = row.value;
                    wire.suffix = row.suffix;
                    wire
                })
                .collect();
        }
        let mut status = GatewayDraftStatus::InProgress;
        let mut label = "Preparing transaction".to_owned();
        let mut message = if let Some(step) = progress
            .steps
            .iter()
            .find(|step| step.status == PublicActionStepStatus::Pending)
        {
            label = private_broadcaster_stage_label(step.stage, progress.requires_device_approval)
                .into();
            step.message
                .as_deref()
                .unwrap_or_else(|| {
                    private_broadcaster_stage_detail(
                        step.stage,
                        step.status,
                        progress.requires_device_approval,
                    )
                })
                .into()
        } else {
            String::new()
        };
        if let Some(result) = &progress.result {
            match &result.result {
                PublicBroadcasterResultKind::Submitted { tx_hash } => {
                    projection.result = Some(GatewayPrivateDraftResult::Submitted);
                    projection.transaction_hash = Some(tx_hash.clone());
                    status = GatewayDraftStatus::Done;
                    label = "Submitted".into();
                    message =
                        "The broadcaster submitted the transaction. Check its on-chain status."
                            .into();
                }
                PublicBroadcasterResultKind::Failed { error } => {
                    projection.result = Some(GatewayPrivateDraftResult::Failed);
                    status = GatewayDraftStatus::Failed;
                    label = "Failed".into();
                    message.clone_from(error);
                }
                PublicBroadcasterResultKind::TimedOut => {
                    projection.result = Some(GatewayPrivateDraftResult::TimedOut);
                    status = GatewayDraftStatus::Failed;
                    label = "Timed out".into();
                    message = "No broadcaster response. The transaction may still be submitted; check history before creating another transaction.".into();
                }
            }
        } else if progress.stopped {
            projection.result = Some(GatewayPrivateDraftResult::Stopped);
            status = GatewayDraftStatus::Failed;
            label = "Stopped".into();
            message = "Desktop processing stopped. A published transaction may still be submitted; check history before creating another transaction.".into();
        } else if let Some(error) = &progress.error {
            projection.result = Some(GatewayPrivateDraftResult::Failed);
            status = GatewayDraftStatus::Failed;
            label = "Failed".into();
            message = error.to_string();
        } else if let Some(result) = &progress.self_broadcast_result {
            projection.transaction_hash = Some(result.tx.tx_hash().to_owned());
            projection.result = Some(GatewayPrivateDraftResult::Submitted);
            status = GatewayDraftStatus::Done;
            label = "Submitted".into();
            message = "Check the desktop app for transaction receipt details.".into();
        }
        projection.favorite = projection.result == Some(GatewayPrivateDraftResult::Submitted)
            && address.is_some_and(|address| {
                !self.is_banned_broadcaster(address) && !self.is_favorite_broadcaster(address)
            });
        execution.update_private(
            status,
            label,
            message,
            status == GatewayDraftStatus::Failed,
            projection,
        );
    }

    pub(in crate::root) fn gateway_private_control(
        &mut self,
        execution: &GatewayDraftExecution,
        execution_id: &str,
        control: GatewayPrivateDraftControl,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = &self.private_broadcaster_progress else {
            return;
        };
        if progress
            .gateway_execution
            .as_ref()
            .is_none_or(|owner| !owner.same_execution(execution))
        {
            return;
        }
        self.publish_gateway_private_progress();
        let Some(current) = execution
            .snapshot()
            .private
            .filter(|current| current.execution_id == execution_id)
        else {
            return;
        };
        let address = public_broadcaster_progress_address(progress).map(str::to_owned);
        match control {
            GatewayPrivateDraftControl::Stop if current.stop => {
                self.stop_private_broadcaster_progress(cx);
            }
            GatewayPrivateDraftControl::StopWaiting if current.stop_waiting => {
                self.stop_private_broadcaster_progress(cx);
            }
            GatewayPrivateDraftControl::Ban if current.ban => {
                if let Some(address) = address {
                    self.add_banned_broadcaster(&address, cx);
                }
            }
            GatewayPrivateDraftControl::Favorite if current.favorite => {
                if let Some(address) = address {
                    self.add_favorite_broadcaster(&address, cx);
                }
            }
            _ => {}
        }
        self.publish_gateway_private_progress();
    }
}
