//! Browser progress follows the native operation, including after its dialog is hidden.
use super::*;
use wallet_ops::gateway::{
    GatewayDraftExecution, GatewayDraftStatus, GatewayPrivateDisplayRow,
    GatewayPrivateDraftControl, GatewayPrivateDraftResult,
};

pub(in crate::root) fn gateway_self_broadcast_result(
    progress: &PrivateBroadcasterProgressState,
) -> Option<(GatewayPrivateDraftResult, Option<String>)> {
    if let Some(outcome) = &progress.sponsored_self_broadcast_outcome {
        return Some(match outcome {
            SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) => (
                if receipt.status {
                    GatewayPrivateDraftResult::Confirmed
                } else {
                    GatewayPrivateDraftResult::Reverted
                },
                Some(receipt.tx_hash.clone()),
            ),
            SponsoredSelfBroadcastSessionOutcome::Stopped {
                bundle_was_accepted,
                ..
            } => (
                if *bundle_was_accepted {
                    GatewayPrivateDraftResult::InclusionUnknown
                } else {
                    GatewayPrivateDraftResult::Stopped
                },
                None,
            ),
        });
    }
    progress.self_broadcast_result.as_ref().map(|result| {
        (
            match result.tx.receipt() {
                Some(receipt) if receipt.status => GatewayPrivateDraftResult::Confirmed,
                Some(_) => GatewayPrivateDraftResult::Reverted,
                None => GatewayPrivateDraftResult::InclusionUnknown,
            },
            Some(result.tx.tx_hash().to_owned()),
        )
    })
}

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
        projection.stop = private_broadcaster_progress_footer_action(progress)
            == ProgressFooterAction::Stop
            && progress.stop_available;
        projection.stop_retries = projection.stop && progress.sponsored_funding;
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
        if progress.flow == PrivateSubmissionProgressFlow::SelfBroadcast {
            projection.context = self_broadcast_progress_context_rows(progress)
                .into_iter()
                .map(|row| {
                    let mut wire = GatewayPrivateDisplayRow::default();
                    wire.label = row.label;
                    wire.value = row.value;
                    wire.suffix = row.suffix;
                    wire
                })
                .collect();
            if let Some(attempt) = progress.self_broadcast_attempts.last() {
                projection.transaction_hash = Some(attempt.tx_hash.clone());
            }
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
        } else if let Some((result, hash)) = gateway_self_broadcast_result(progress) {
            projection.result = Some(result);
            if hash.is_some() {
                projection.transaction_hash = hash;
            }
            (status, label, message) = match result {
                GatewayPrivateDraftResult::Confirmed => (GatewayDraftStatus::Done, "Confirmed".into(), "The transaction was confirmed on chain.".into()),
                GatewayPrivateDraftResult::Reverted => (GatewayDraftStatus::Failed, "Reverted".into(), "The transaction reverted on chain. Check the desktop history for details.".into()),
                GatewayPrivateDraftResult::InclusionUnknown => (GatewayDraftStatus::Failed, "Inclusion unknown".into(), "The transaction may still be included. Its private inputs remain reserved; check history before creating another transaction.".into()),
                _ => (GatewayDraftStatus::Failed, "Stopped".into(), "Local processing stopped before a relay accepted the bundle.".into()),
            };
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
        } else if progress::private_self_broadcast_requires_attention(progress) {
            status = GatewayDraftStatus::Attention;
            label = "Review gas in the desktop app".into();
            progress
                .self_broadcast_action_error
                .as_deref()
                .or_else(|| {
                    progress
                        .steps
                        .iter()
                        .find(|step| step.status == PublicActionStepStatus::Error)
                        .and_then(|step| step.message.as_deref())
                })
                .unwrap_or("This transaction step needs attention in the desktop app.")
                .clone_into(&mut message);
        }
        projection.favorite = projection.result == Some(GatewayPrivateDraftResult::Submitted)
            && address.is_some_and(|address| {
                !self.is_banned_broadcaster(address) && !self.is_favorite_broadcaster(address)
            });
        execution.update_private(
            status,
            label,
            message,
            status == GatewayDraftStatus::Failed || status == GatewayDraftStatus::Attention,
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
