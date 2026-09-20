use super::{
    AppContext, Arc, Context, DesktopPrivateSpendAuthorization, Disableable, Entity, ExecutorAsset,
    ExecutorOperationId, ExecutorRecord, Focusable, InputEvent, InputState, ParentElement,
    SpendAuthorizationSummary, SpendAuthorizationSummaryRow, StealthAccountsView, StealthAction,
    Styled, Task, U256, Window, app_button, app_input, app_muted_text, app_segment_button,
    app_strong_text, div, labeled_field,
};
use gpui::{App, InteractiveElement as _, prelude::FluentBuilder as _};
use gpui_component::{
    button::{ButtonGroup, ButtonVariants as _},
    select::{SearchableVec, SelectEvent, SelectState},
};
use ui::gas_fee::{GasFeeEditTarget, GasFeeEditor, GasFeeEditorEvent, GasFeeMode};
use wallet_ops::{
    ExecutorPaidRecoveryRequest, ExecutorRecoveryApproval, ExecutorRecoveryExecution,
    ExecutorRecoveryFeeEstimate, ExecutorRecoveryFunding, PreparedExecutorRecovery,
    PreparedExecutorRecoveryRetry, PublicActionGasFeeSelection, PublicActionProgressUpdate,
    PublicBroadcasterCandidate, PublicBroadcasterResultKind, SelfBroadcastGasFeeQuote,
    TransactionGenerationStage, WakuDeliveryClient, vault::ExecutorPayloadStatus,
};

use crate::root::gas_fee::{GasRetryInputs, format_gwei, parse_gwei_to_wei};
use crate::root::public_balances::public_asset_decimals;

mod assets;
mod broadcaster;
mod progress;
mod retry;

use crate::root::public_action::PublicActionStepStatus;
use progress::{RecoveryProgress, RecoveryProgressSource, recovery_execution_status};

use crate::root::public_broadcaster::PublicBroadcasterFeeTokenOption;
use assets::RecoveryAssetItem;

#[derive(Clone)]
pub(super) enum RecoveryAuthorization {
    Retry {
        prepared: Arc<PreparedExecutorRecoveryRetry>,
    },
    Prepare {
        approval: Arc<ExecutorRecoveryApproval>,
    },
    Submit {
        prepared: Arc<PreparedExecutorRecovery>,
        step: usize,
        waku: Option<Arc<WakuDeliveryClient>>,
    },
}

pub(super) struct RecoveryForm {
    pub(super) open: bool,
    pub(super) asset: Option<ExecutorAsset>,
    pub(super) asset_select: Entity<SelectState<SearchableVec<RecoveryAssetItem>>>,
    asset_items: Vec<RecoveryAssetItem>,
    pub(super) amount: Entity<InputState>,
    retry_gas_limit: Entity<InputState>,
    pub(super) native_funding: bool,
    gas: GasRetryInputs,
    gas_mode: GasFeeMode,
    pub(super) gas_quote: Option<SelfBroadcastGasFeeQuote>,
    fee_token: Option<super::Address>,
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    fee_estimate: Option<ExecutorRecoveryFeeEstimate>,
    fee_breakdown_open: bool,
    fee_error: Option<String>,
    estimate_candidate: Option<PublicBroadcasterCandidate>,
    estimate_revision: u64,
    estimate_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    next_estimate: std::time::Instant,
    allow_out_of_range: bool,
    favorites_only: bool,
    candidates: Vec<PublicBroadcasterCandidate>,
    selected_broadcaster: Option<String>,
    pub(super) prepared: Option<Arc<PreparedExecutorRecovery>>,
    progress: Option<Task<()>>,
    dialog: Option<RecoveryProgress>,
}

impl RecoveryForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, StealthAccountsView>) -> Self {
        let amount = cx.new(|cx| InputState::new(window, cx));
        let retry_gas_limit = cx.new(|cx| InputState::new(window, cx));
        let asset_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::<RecoveryAssetItem>::new(Vec::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        cx.subscribe_in(&asset_select, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(asset)) = event
                && this.job.is_none()
            {
                this.set_recovery_asset(*asset, window, cx);
            }
        })
        .detach();
        let gas = GasRetryInputs::new(1_000_000_000, 1_000_000_000, window, cx);
        for input in [
            &amount,
            &retry_gas_limit,
            &gas.max_fee_input,
            &gas.max_tip_input,
        ] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate_recovery();
                    this.schedule_recovery_estimate(cx);
                    cx.notify();
                }
            })
            .detach();
        }
        Self {
            open: false,
            asset: None,
            asset_select,
            asset_items: Vec::new(),
            amount,
            retry_gas_limit,
            native_funding: true,
            gas,
            gas_mode: GasFeeMode::Auto,
            gas_quote: None,
            fee_token: None,
            fee_options: Vec::new(),
            fee_estimate: None,
            fee_breakdown_open: true,
            fee_error: None,
            estimate_candidate: None,
            estimate_revision: 0,
            estimate_task: None,
            refresh_task: None,
            next_estimate: std::time::Instant::now(),
            allow_out_of_range: false,
            favorites_only: false,
            candidates: Vec::new(),
            selected_broadcaster: None,
            prepared: None,
            progress: None,
            dialog: None,
        }
    }
}

impl StealthAccountsView {
    pub(super) fn select_recovery_account(
        &mut self,
        operation: ExecutorOperationId,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(_record) = self
            .records
            .iter()
            .find(|record| record.operation() == operation)
        else {
            self.error =
                Some("The saved stealth account is unavailable. Reopen local accounts.".into());
            cx.notify();
            return;
        };
        self.selected = Some(operation);
        self.recovery.dialog = None;
        self.invalidate_recovery();
        cx.notify();
    }

    pub(super) fn focus_recovery_amount(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.recovery
            .amount
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    pub(super) fn set_recovery_asset(
        &mut self,
        asset: ExecutorAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.recovery.asset = Some(asset);
        self.recovery.asset_select.update(cx, |select, cx| {
            select.set_selected_value(&asset, window, cx);
        });
        let amount = self
            .selected
            .and_then(|operation| self.observations.get(&operation))
            .and_then(|observations| observations.assets.get(&asset))
            .and_then(|balance| balance.value)
            .map_or_else(String::new, |value| {
                self.asset_decimals(asset, cx).map_or_else(
                    || value.amount.to_string(),
                    |decimals| railgun_ui::format_scaled_amount(value.amount, decimals),
                )
            });
        self.recovery
            .amount
            .update(cx, |input, cx| input.set_value(amount, window, cx));
        self.invalidate_recovery();
        self.schedule_recovery_estimate(cx);
        cx.notify();
    }

    pub(super) fn finish_recovery_progress(&mut self) {
        self.recovery.progress = None;
    }

    pub(super) fn render_recovery_form(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let mut form = div().w_full().min_w_0().flex().flex_col().gap_3();
        let Some(record) = self
            .records
            .iter()
            .find(|record| Some(record.operation()) == self.selected)
        else {
            return form;
        };
        let busy = self.job.is_some();
        if matches!(self.recovery.asset, Some(ExecutorAsset::Erc721 { .. })) {
            form = form.child(app_muted_text("Recover exactly the selected NFT."));
        } else {
            let known_units = self
                .recovery
                .asset
                .and_then(|asset| self.asset_decimals(asset, cx))
                .is_some();
            form = form.child(labeled_field(
                if self.recovery.asset == Some(ExecutorAsset::Native)
                    && self.recovery.native_funding
                {
                    "Maximum amount to shield after reserving gas"
                } else if known_units {
                    "Amount to shield"
                } else {
                    "Amount to shield (smallest token units)"
                },
                div()
                    .debug_selector(|| "stealth-recovery-amount".into())
                    .child(app_input(&self.recovery.amount).disabled(busy)),
            ));
        }
        form = form.child(
            ButtonGroup::new("stealth-recovery-funding")
                .outline()
                .compact()
                .w_full()
                .children(
                    [
                        (true, "Native gas", "stealth-native-funding"),
                        (false, "Public broadcaster", "stealth-paid-funding"),
                    ]
                    .into_iter()
                    .map(|(native, label, id)| {
                        app_segment_button(
                            id,
                            label,
                            self.recovery.native_funding == native,
                            busy || native && !self.recovery_has_native_balance(),
                            None,
                        )
                        .flex_1()
                        .min_w_0()
                        .debug_selector(move || id.to_owned())
                        .when(native && !self.recovery_has_native_balance(), |button| {
                            button.tooltip(
                                "Check that this account has a positive native balance to pay gas.",
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if native && !this.recovery_has_native_balance() {
                                return;
                            }
                            this.recovery.native_funding = native;
                            this.ensure_recovery_network(cx);
                            this.invalidate_recovery();
                            this.schedule_recovery_estimate(cx);
                            cx.notify();
                        }))
                    }),
                ),
        );
        if self.recovery.native_funding {
            let view = cx.entity();
            form = form.child(
                GasFeeEditor::new(
                    "stealth-gas",
                    &self.recovery.gas.max_fee_input,
                    &self.recovery.gas.max_tip_input,
                    move |event, window, cx| {
                        view.update(cx, |this, cx| {
                            this.invalidate_recovery();
                            match event {
                                GasFeeEditorEvent::Refresh => this.refresh_recovery_gas(cx),
                                GasFeeEditorEvent::Mode(mode) => this.recovery.gas_mode = *mode,
                                GasFeeEditorEvent::Edit(target) => {
                                    if let Some(quote) = this.recovery.gas_quote {
                                        this.recovery.gas.max_fee_input.update(cx, |input, cx| {
                                            input.set_value(
                                                format_gwei(quote.suggested_max_fee_per_gas),
                                                window,
                                                cx,
                                            );
                                        });
                                        this.recovery.gas.max_tip_input.update(cx, |input, cx| {
                                            input.set_value(
                                                format_gwei(
                                                    quote.suggested_max_priority_fee_per_gas,
                                                ),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                    this.recovery.gas_mode = GasFeeMode::Custom;
                                    let input = match target {
                                        GasFeeEditTarget::MaxFee => {
                                            &this.recovery.gas.max_fee_input
                                        }
                                        GasFeeEditTarget::MaxTip => {
                                            &this.recovery.gas.max_tip_input
                                        }
                                    };
                                    input.read(cx).focus_handle(cx).focus(window, cx);
                                }
                            }
                            cx.notify();
                        });
                    },
                )
                .mode(self.recovery.gas_mode)
                .quote(self.recovery.gas_quote.map(|quote| {
                    (
                        format_gwei(quote.suggested_max_fee_per_gas),
                        format_gwei(quote.suggested_max_priority_fee_per_gas),
                    )
                }))
                .disabled(busy),
            );
            form = form.child(app_muted_text("This account pays gas. Native recovery reserves the approved gas budget before wrapping; a remainder may stay public."));
            if let Some(error) = &self.recovery.fee_error {
                form = form.child(app_muted_text(error.clone()).whitespace_normal());
            } else if self.recovery.gas_quote.is_none() {
                form = form.child(app_muted_text("Estimating gas fees…"));
            }
        } else {
            form = form.child(self.render_recovery_broadcaster_settings(cx));
        }
        form = form.child(
            app_button(
                "stealth-prepare-recovery",
                if self.recovery.prepared.is_some() {
                    "Review recovery…"
                } else {
                    "Recover…"
                },
            )
            .debug_selector(|| "stealth-prepare-recovery".into())
            .primary()
            .disabled(
                busy || self
                    .recovery
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| self.next_recovery_step(prepared).is_none())
                    || self.recovery.prepared.is_none()
                        && if self.recovery.native_funding {
                            !self.recovery_has_native_balance()
                                || self.recovery.gas_quote.is_none()
                                    && self.recovery.gas_mode == GasFeeMode::Auto
                        } else {
                            self.recovery.fee_estimate.is_none()
                        },
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.recovery.prepared.is_some() {
                    this.request_recovery_submission(None, window, cx);
                } else {
                    this.request_recovery_preparation(window, cx);
                }
            })),
        );
        if self.recovery.dialog.is_some() {
            form = form.child(
                app_button("stealth-recovery-show-progress", "View progress")
                    .debug_selector(|| "stealth-recovery-show-progress".into())
                    .outline()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_recovery_progress(window, cx);
                    })),
            );
        }
        form = form.children(self.render_recovery_retries(record, cx));
        form
    }

    pub(super) fn continue_recovery(
        &mut self,
        action: RecoveryAuthorization,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.continue_recovery_with_progress(action, authorization, true, window, cx);
    }

    fn continue_recovery_with_progress(
        &mut self,
        action: RecoveryAuthorization,
        authorization: DesktopPrivateSpendAuthorization,
        show_progress: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let owner = Arc::clone(&self.owner);
        match action {
            RecoveryAuthorization::Retry { prepared } => {
                self.continue_recovery_retry(prepared, authorization, window, cx);
            }
            RecoveryAuthorization::Prepare { approval } => {
                self.start_recovery_job(
                    RecoveryProgressSource::Preparation,
                    show_progress,
                    async move {
                        let operation = approval.operation();
                        let asset = approval.asset();
                        let amount = approval.amount();
                        let funding = approval.funding().clone();
                        let prepared = if asset == ExecutorAsset::Native
                            && matches!(funding, ExecutorRecoveryFunding::ExecutorNative { .. })
                        {
                            owner
                                .prepare_native_recovery_up_to(
                                    operation,
                                    amount,
                                    funding,
                                    &authorization,
                                )
                                .await
                        } else {
                            owner
                                .prepare_recovery(operation, asset, amount, funding, &authorization)
                                .await
                        }
                        .map(Arc::new)?;
                        Ok((prepared, authorization, approval))
                    },
                    |this, (prepared, authorization, approval), window, cx| {
                        this.recovery.prepared = Some(prepared);
                        if let Some(dialog) = &mut this.recovery.dialog {
                            dialog
                                .finish(PublicActionStepStatus::Done, "Recovery prepared.".into());
                        }
                        this.request_recovery_submission(
                            Some((authorization, approval)),
                            window,
                            cx,
                        );
                    },
                    window,
                    cx,
                );
            }
            RecoveryAuthorization::Submit {
                prepared,
                step,
                waku,
            } => {
                if !self
                    .recovery
                    .prepared
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &prepared))
                {
                    return;
                }
                let session = Arc::clone(&self.session);
                if prepared.execution() == ExecutorRecoveryExecution::Ordinary
                    || matches!(
                        prepared.execution(),
                        ExecutorRecoveryExecution::SignedMulticall { .. }
                    )
                {
                    let (progress, receiver) =
                        tokio::sync::watch::channel(None::<PublicActionProgressUpdate>);
                    self.start_recovery_job(RecoveryProgressSource::Native(receiver), show_progress, async move {
                        let report = move |update| { let _ = progress.send(Some(update)); };
                        let outcome = match prepared.execution() {
                            ExecutorRecoveryExecution::Ordinary => Box::pin(owner.submit_ordinary_recovery_step(&prepared, step, &authorization, report)).await?,
                            _ => Box::pin(owner.submit_native_recovery_batch(&prepared, &authorization, report)).await?,
                        };
                        Ok((format!("{} Check the private receipt before considering recovery complete.", payload_status(outcome.status)), recovery_execution_status(outcome.status)))
                    }, |this, (status, result), _, _| {
                        if let Some(dialog) = &mut this.recovery.dialog {
                            dialog.finish(result, status);
                        }
                    }, window, cx);
                } else {
                    let Some(waku) = waku else {
                        self.error =
                            Some("Connect to the broadcaster network before submitting.".into());
                        cx.notify();
                        return;
                    };
                    let (progress, receiver) = tokio::sync::watch::channel(
                        TransactionGenerationStage::SelectingPrivateNotes,
                    );
                    self.start_recovery_job(
                        RecoveryProgressSource::Broadcaster(receiver),
                        show_progress,
                        async move {
                            let outcome = owner
                                .submit_paid_recovery(ExecutorPaidRecoveryRequest {
                                    recovery: prepared,
                                    session,
                                    authorization,
                                    waku,
                                    verify_proof: true,
                                    progress_tx: Some(progress),
                                    response_timeout: std::time::Duration::from_mins(2),
                                    republish_interval: std::time::Duration::from_secs(5),
                                })
                                .await?;
                            Ok(outcome.result)
                        },
                        |this, result, _, _| {
                            if let Some(dialog) = &mut this.recovery.dialog {
                                dialog.finish_broadcaster(result);
                            }
                        },
                        window,
                        cx,
                    );
                }
            }
        }
    }

    fn next_recovery_step(&self, prepared: &PreparedExecutorRecovery) -> Option<usize> {
        if prepared.execution() != ExecutorRecoveryExecution::Ordinary {
            return Some(0);
        }
        let record = self
            .records
            .iter()
            .find(|record| record.operation() == prepared.operation())?;
        (0..prepared.calls().len()).find(|step| {
            !record.recovery_transactions().iter().any(|transaction| {
                transaction.recovery() == prepared.recovery()
                    && transaction.step() as usize == *step
                    && record.recovery_transaction_status(transaction.hash())
                        == Some(ExecutorPayloadStatus::Executed)
            })
        })
    }

    fn request_recovery_submission(
        &mut self,
        authorization: Option<(
            DesktopPrivateSpendAuthorization,
            Arc<ExecutorRecoveryApproval>,
        )>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(prepared) = self.recovery.prepared.clone() else {
            return;
        };
        let Some(step) = self.next_recovery_step(&prepared) else {
            return;
        };
        let record = match self.owner.validate_recovery(&prepared) {
            Ok(record) => record,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let approved = authorization
            .as_ref()
            .is_some_and(|(_, approval)| approval.covers(&prepared, &record));
        let show_progress = authorization.is_none()
            || self.recovery.open
                && self
                    .recovery
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.open);
        if !approved && !show_progress {
            return;
        }
        self.close_recovery_progress(window, cx);
        let mut rows = vec![
            SpendAuthorizationSummaryRow::new("Source", prepared.source().to_string())
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Private destination", prepared.recipient())
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Asset", self.asset_name(prepared.asset(), cx)),
            SpendAuthorizationSummaryRow::new(
                "Amount to shield",
                self.recovery_amount_label(prepared.asset(), prepared.amount(), cx),
            ),
            SpendAuthorizationSummaryRow::new("Funding", funding_label(prepared.funding())),
        ];
        if prepared.changes_delegation() {
            if let Some(delegate) = prepared.current_delegate() {
                rows.push(
                    SpendAuthorizationSummaryRow::new(
                        "Current delegation",
                        delegate.to_checksum(None),
                    )
                    .with_shortened_copyable(),
                );
            }
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Authorize delegation",
                    prepared.delegate().to_checksum(None),
                )
                .with_shortened_copyable(),
            );
            rows.push(SpendAuthorizationSummaryRow::new(
                "Delegation",
                "Delegation remains installed even if the recovery transaction reverts.",
            ));
        }
        if let Some(nonce) = prepared.replacement_nonce() {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Earlier recovery attempt",
                match prepared.funding() {
                    ExecutorRecoveryFunding::ExecutorNative { .. } => format!(
                        "Replace the account transaction at nonce {nonce} with this recovery batch."
                    ),
                    ExecutorRecoveryFunding::PublicBroadcaster { .. } => {
                        "An earlier account transaction may still execute before this recovery."
                            .into()
                    }
                },
            ));
        }
        rows.extend(self.recovery_fee_authorization_rows(
            prepared.funding(),
            prepared.maximum_native_fee(),
            cx,
        ));
        let waku = match prepared.funding() {
            ExecutorRecoveryFunding::ExecutorNative { .. } => {
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Signing",
                    if prepared.execution() == ExecutorRecoveryExecution::Ordinary {
                        format!(
                            "Step {} of {}: {}",
                            step + 1,
                            prepared.calls().len(),
                            step_label(prepared.steps()[step])
                        )
                    } else {
                        "Shield the selected asset in one atomic transaction".into()
                    },
                ));
                None
            }
            ExecutorRecoveryFunding::PublicBroadcaster { .. } => {
                let waku = self
                    .root
                    .update(cx, |root, cx| {
                        root.ensure_waku_for_delivery(
                            crate::root::DeliveryMode::PublicBroadcaster,
                            cx,
                        );
                        root.active_waku()
                    })
                    .ok()
                    .flatten();
                if waku.is_none() {
                    self.error = Some(
                        "Wait for the broadcaster network connection, then review again.".into(),
                    );
                    cx.notify();
                    return;
                }
                waku
            }
        };
        let summary = SpendAuthorizationSummary::new(
            "Recover to private balance",
            "Shield only this approved amount.",
            rows,
        );
        let action = RecoveryAuthorization::Submit {
            prepared,
            step,
            waku,
        };
        if let Some((authorization, _)) = authorization {
            if approved {
                self.continue_recovery_with_progress(
                    action,
                    authorization,
                    show_progress,
                    window,
                    cx,
                );
                return;
            }
            let command = Arc::new(super::StealthAuthorization {
                session: Arc::clone(&self.session),
                action: StealthAction::Recover(action),
            });
            self.pending_authorization = Some(Arc::clone(&command));
            let view = cx.entity();
            let _ = self.root.update(cx, |root, cx| {
                if root.stealth_session_is_current(&command.session) {
                    root.open_prepared_spend_review(
                        crate::root::spend_authorization::SpendAuthorizationIntent::StealthAccounts(
                            view, command,
                        ),
                        summary.requiring_explicit_review(),
                        authorization,
                        window,
                        cx,
                    );
                }
            });
        } else {
            self.request_authorization(StealthAction::Recover(action), summary, window, cx);
        }
    }

    pub(super) fn invalidate_recovery(&mut self) {
        self.pending_authorization = None;
        self.recovery.prepared = None;
        self.recovery.estimate_candidate = None;
        self.recovery.fee_estimate = None;
        self.recovery.fee_error = None;
        self.recovery.estimate_revision = self.recovery.estimate_revision.wrapping_add(1);
        self.recovery.estimate_task = None;
        self.error = None;
    }

    pub(super) fn asset_decimals(&self, asset: ExecutorAsset, cx: &App) -> Option<u8> {
        match asset {
            ExecutorAsset::Native => self.root.upgrade().and_then(|root| {
                root.read(cx)
                    .effective_chain_configs
                    .get(self.session.chain_id)
                    .map(|chain| chain.native_currency.decimals)
            }),
            ExecutorAsset::Erc721 { .. } => Some(0),
            ExecutorAsset::Erc20(token) => self.root.upgrade().and_then(|root| {
                public_asset_decimals(
                    root.read(cx)
                        .effective_chain_configs
                        .get(self.session.chain_id),
                    wallet_ops::PublicAssetId::Erc20(token),
                    Some(&root.read(cx).effective_token_registry),
                )
            }),
        }
    }

    pub(super) fn recovery_amount_label(
        &self,
        asset: ExecutorAsset,
        amount: U256,
        cx: &App,
    ) -> String {
        self.asset_decimals(asset, cx).map_or_else(
            || format!("{amount} smallest token units"),
            |decimals| railgun_ui::format_scaled_amount(amount, decimals),
        )
    }

    fn recovery_gas(&self, cx: &App) -> Result<PublicActionGasFeeSelection, String> {
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            if self.recovery.gas_mode == GasFeeMode::Auto {
                let quote = self
                    .recovery
                    .gas_quote
                    .ok_or("Refresh gas fees before preparing recovery.")?;
                (
                    quote.suggested_max_fee_per_gas,
                    quote.suggested_max_priority_fee_per_gas,
                )
            } else {
                (
                    parse_gwei_to_wei(&self.recovery.gas.max_fee_input.read(cx).value())?,
                    parse_gwei_to_wei(&self.recovery.gas.max_tip_input.read(cx).value())?,
                )
            };
        Ok(PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    }

    fn refresh_recovery_gas(&mut self, cx: &mut Context<'_, Self>) {
        self.recovery.gas_quote = None;
        self.invalidate_recovery();
        self.schedule_recovery_estimate(cx);
    }

    fn recovery_fee_authorization_rows(
        &self,
        funding: &ExecutorRecoveryFunding,
        maximum_native_fee: U256,
        cx: &App,
    ) -> Vec<SpendAuthorizationSummaryRow> {
        match funding {
            ExecutorRecoveryFunding::ExecutorNative { gas_fee } => {
                let mut rows = vec![SpendAuthorizationSummaryRow::new(
                    "Maximum network fee",
                    crate::root::format_native_token_amount_for_display(
                        self.session.chain_id,
                        maximum_native_fee,
                    ),
                )];
                if let PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                } = gas_fee
                {
                    rows.push(SpendAuthorizationSummaryRow::new(
                        "Max fee / priority fee",
                        format!(
                            "{} / {} gwei",
                            format_gwei(*max_fee_per_gas),
                            format_gwei(*max_priority_fee_per_gas)
                        ),
                    ));
                }
                rows
            }
            ExecutorRecoveryFunding::PublicBroadcaster {
                candidate,
                maximum_private_fee,
            } => {
                let asset = ExecutorAsset::Erc20(candidate.token);
                vec![SpendAuthorizationSummaryRow::new(
                    "Maximum private fee",
                    format!(
                        "{} {}",
                        self.recovery_amount_label(asset, *maximum_private_fee, cx),
                        self.asset_name(asset, cx)
                    ),
                )]
            }
        }
    }

    fn request_recovery_preparation(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.refresh_recovery_controls(window, cx);
        let result = (|| {
            let operation = self.selected.ok_or("Select a historical account.")?;
            let asset = self.recovery.asset.ok_or("Select an asset to recover.")?;
            let amount = if matches!(asset, ExecutorAsset::Erc721 { .. }) {
                U256::ONE
            } else {
                wallet_ops::parse_send_amount(
                    &self.recovery.amount.read(cx).value(),
                    self.asset_decimals(asset, cx),
                )
                .map_err(|error| error.to_string())?
            };
            let funding = if self.recovery.native_funding {
                if !self.recovery_has_native_balance() {
                    return Err("This account needs a positive native balance to pay gas.".into());
                }
                ExecutorRecoveryFunding::ExecutorNative {
                    gas_fee: self.recovery_gas(cx)?,
                }
            } else {
                let estimate = self
                    .recovery
                    .fee_estimate
                    .as_ref()
                    .ok_or("Wait for the broadcaster fee estimate.")?;
                let candidate = estimate.broadcaster();
                if !self
                    .recovery
                    .candidates
                    .iter()
                    .any(|current| broadcaster::same_offer(current, candidate))
                {
                    return Err("The broadcaster quote changed. Wait for a new estimate.".into());
                }
                ExecutorRecoveryFunding::PublicBroadcaster {
                    candidate: Box::new(candidate.clone()),
                    maximum_private_fee: estimate.fee_amount(),
                }
            };
            self.owner
                .recovery_approval(operation, asset, amount, funding)
                .map_err(|error| error.to_string())
        })();
        let approval = match result {
            Ok(result) => result,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let mut rows = vec![
            SpendAuthorizationSummaryRow::new("Source", approval.source().to_checksum(None))
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Private destination", approval.recipient())
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Asset", self.asset_name(approval.asset(), cx)),
            SpendAuthorizationSummaryRow::new(
                if approval.asset() == ExecutorAsset::Native && self.recovery.native_funding {
                    "Up to, after reserving gas"
                } else {
                    "Amount"
                },
                self.recovery_amount_label(approval.asset(), approval.amount(), cx),
            ),
            SpendAuthorizationSummaryRow::new("Funding", funding_label(approval.funding())),
        ];
        rows.extend(self.recovery_fee_authorization_rows(
            approval.funding(),
            approval.maximum_native_fee(),
            cx,
        ));
        let summary = SpendAuthorizationSummary::new(
            "Recover to private balance",
            "Shield the selected asset in one atomic transaction.",
            rows,
        )
        .with_confirm_label("Authorize recovery");
        self.request_authorization(
            StealthAction::Recover(RecoveryAuthorization::Prepare {
                approval: Arc::new(approval),
            }),
            summary,
            window,
            cx,
        );
    }
}

fn funding_label(funding: &ExecutorRecoveryFunding) -> String {
    match funding {
        ExecutorRecoveryFunding::ExecutorNative { .. } => "This account's native balance".into(),
        ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } => format!(
            "Private fee paid to {}",
            crate::root::broadcaster_picker::broadcaster_candidate_label(candidate)
        ),
    }
}

const fn step_label(step: wallet_ops::PublicActionProgressStep) -> &'static str {
    match step {
        wallet_ops::PublicActionProgressStep::Wrap => "Wrap native currency",
        wallet_ops::PublicActionProgressStep::Approve => "Approve the exact shield amount",
        wallet_ops::PublicActionProgressStep::Shield => "Shield to private balance",
        _ => "Recover assets",
    }
}

const fn payload_status(status: ExecutorPayloadStatus) -> &'static str {
    match status {
        ExecutorPayloadStatus::Uncertain => "Execution is not yet confirmed",
        ExecutorPayloadStatus::Reverted => "Transaction reverted",
        ExecutorPayloadStatus::MissingEffects => "Receipt found; expected effects are missing",
        ExecutorPayloadStatus::Executed => "Expected execution effects confirmed",
        ExecutorPayloadStatus::Invalidated { .. } => "Another recorded payload consumed this nonce",
    }
}
