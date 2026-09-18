use alloy::primitives::B256;
use gpui::{Context, ParentElement, SharedString, Styled, Window, div};
use gpui_component::{Disableable, scroll::ScrollableElement as _};
use wallet_ops::vault::{ExecutorOperationId, ExecutorRecoveryStepKind};

use super::{
    Arc, DesktopPrivateSpendAuthorization, ExecutorPayloadStatus, ExecutorRecord,
    PreparedExecutorRecoveryRetry, PublicActionGasFeeSelection, PublicActionProgressUpdate,
    RecoveryAuthorization, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    StealthAccountsView, StealthAction, app_button, app_input, app_muted_text, format_gwei,
    labeled_field, payload_status,
};

impl StealthAccountsView {
    pub(super) fn render_recovery_retries(
        &self,
        record: &ExecutorRecord,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if !self.recovery.native_funding {
            return None;
        }
        let pending = record
            .recovery_transactions()
            .iter()
            .rev()
            .filter(|issued| {
                record
                    .recovery_transaction_status(issued.hash())
                    .unwrap_or(ExecutorPayloadStatus::Uncertain)
                    == ExecutorPayloadStatus::Uncertain
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return None;
        }
        let section = div().w_full().min_w_0().flex().flex_col().gap_2()
                .child(labeled_field(
                    "Retry gas limit (blank keeps the original limit)",
                    app_input(&self.recovery.retry_gas_limit).disabled(self.job.is_some()),
                ))
                .child(app_muted_text("Retry reprices the same pending transaction. After a confirmed failure, check the account and prepare the remaining recovery with its current nonce.").whitespace_normal());
        let mut retries = div()
            .w_full()
            .min_w_0()
            .max_h_64()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .gap_2();
        for issued in pending {
            let operation = record.operation();
            let hash = issued.hash();
            let row = div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    app_muted_text(format!(
                        "{}: {}",
                        recovery_step_label(issued.kind()),
                        issued.hash(),
                    ))
                    .whitespace_normal(),
                )
                .child(
                    app_button(
                        SharedString::from(format!("stealth-retry-{hash}")),
                        "Review retry…",
                    )
                    .outline()
                    .disabled(self.job.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_recovery_retry(operation, hash, window, cx);
                    })),
                );
            retries = retries.child(row);
        }
        Some(section.child(retries))
    }

    fn request_recovery_retry(
        &mut self,
        operation: ExecutorOperationId,
        hash: B256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let result = (|| {
            if !self.recovery.native_funding || self.selected != Some(operation) {
                return Err("Select this account's native gas to retry its transaction.".to_owned());
            }
            let original = self
                .records
                .iter()
                .find(|record| record.operation() == operation)
                .and_then(|record| {
                    record
                        .recovery_transactions()
                        .iter()
                        .find(|issued| issued.hash() == hash)
                })
                .ok_or("The retained recovery transaction is unavailable.")?;
            let value = self.recovery.retry_gas_limit.read(cx).value();
            let gas_limit = if value.trim().is_empty() {
                original
                    .transaction()
                    .gas
                    .ok_or("The original gas limit is unavailable.")?
            } else {
                value
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a positive gas limit.".to_owned())?
            };
            self.owner
                .prepare_ordinary_recovery_retry(operation, hash, self.recovery_gas(cx)?, gas_limit)
                .map(Arc::new)
                .map_err(|error| error.to_string())
        })();
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let original = prepared.original();
        let transaction = original.transaction();
        let PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } = prepared.gas_fee()
        else {
            return;
        };
        let summary = SpendAuthorizationSummary::new(
            "Retry recovery transaction",
            "Sign the retained call at the same account nonce with these gas fees. The recovered asset, amount and shield destination remain fixed in its calldata. This does not submit later recovery steps.",
            vec![
                SpendAuthorizationSummaryRow::new("Step", recovery_step_label(original.kind())),
                SpendAuthorizationSummaryRow::new("Source", prepared.source().to_string()).with_shortened_copyable(),
                SpendAuthorizationSummaryRow::new("Retained transaction", hash.to_string()).with_shortened_copyable(),
                SpendAuthorizationSummaryRow::new("Call destination", transaction.to.as_ref().and_then(|to| to.to()).map_or_else(|| "Unavailable".into(), ToString::to_string)).with_shortened_copyable(),
                SpendAuthorizationSummaryRow::new("Native value", railgun_ui::format_scaled_amount(transaction.value.unwrap_or_default(), 18)),
                SpendAuthorizationSummaryRow::new("Account nonce", transaction.nonce.map_or_else(|| "Unavailable".into(), |nonce| nonce.to_string())),
                SpendAuthorizationSummaryRow::new("Funding", "This account's native balance"),
                SpendAuthorizationSummaryRow::new("Gas limit", prepared.gas_limit().to_string()),
                SpendAuthorizationSummaryRow::new("Max fee / priority fee", format!("{} / {} gwei", format_gwei(max_fee_per_gas), format_gwei(max_priority_fee_per_gas))),
                SpendAuthorizationSummaryRow::new("Native reserve including remaining steps", railgun_ui::format_scaled_amount(prepared.native_reserve(), 18)),
            ],
        ).with_payload("Retained calldata", transaction.input.input().map_or_else(|| "0x".into(), ToString::to_string));
        self.request_authorization(
            StealthAction::Recover(RecoveryAuthorization::Retry { prepared }),
            summary,
            window,
            cx,
        );
    }

    pub(super) fn continue_recovery_retry(
        &mut self,
        prepared: Arc<PreparedExecutorRecoveryRetry>,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let owner = Arc::clone(&self.owner);
        let (progress, receiver) = tokio::sync::watch::channel(None::<PublicActionProgressUpdate>);
        self.start_recovery_job(
            super::RecoveryProgressSource::Retry(receiver),
            true,
            async move {
                let outcome = Box::pin(owner.retry_ordinary_recovery_transaction(
                    &prepared,
                    &authorization,
                    move |update| {
                        let _ = progress.send(Some(update));
                    },
                ))
                .await?;
                Ok((
                    format!(
                        "{}. Inspect the remaining recovery and private receipt before continuing.",
                        payload_status(outcome.status)
                    ),
                    super::recovery_execution_status(outcome.status),
                ))
            },
            |this, (status, result), _, _| {
                this.recovery.prepared = None;
                if let Some(dialog) = &mut this.recovery.dialog {
                    dialog.finish(result, status);
                }
            },
            window,
            cx,
        );
    }
}

const fn recovery_step_label(kind: ExecutorRecoveryStepKind) -> &'static str {
    match kind {
        ExecutorRecoveryStepKind::Wrap => "Wrap native currency",
        ExecutorRecoveryStepKind::ApproveErc20 | ExecutorRecoveryStepKind::ApproveErc721 => {
            "Approve the shield amount"
        }
        ExecutorRecoveryStepKind::Shield => "Shield to private balance",
    }
}
