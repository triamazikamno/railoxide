use super::*;

pub(in crate::root) const PUBLIC_ACTION_RETRY_DEFAULT_FEE_WEI: u128 = 1_000_000_000;

#[derive(Clone)]
pub(in crate::root) struct PublicSendDraft {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) asset: PublicAssetId,
    pub(in crate::root) asset_label: String,
    pub(in crate::root) asset_icon_path: Option<WalletIconSource>,
    pub(in crate::root) asset_decimals: Option<u8>,
    pub(in crate::root) public_account_uuid: Arc<str>,
    pub(in crate::root) public_account_label: String,
    pub(in crate::root) public_account_source: PublicAccountSource,
    pub(in crate::root) view_session: Arc<DesktopViewSession>,
    pub(in crate::root) vault_store: Arc<DesktopVaultStore>,
    pub(in crate::root) amount: U256,
    pub(in crate::root) recipient: Address,
    pub(in crate::root) intent: PublicTransactionIntent,
    pub(in crate::root) advanced_estimate: Option<PublicAdvancedTransactionEstimate>,
    pub(in crate::root) gas_fee: PublicActionGasFeeSelection,
    pub(in crate::root) fee_display: PublicActionFeeDisplay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum AdvancedPublicSendField {
    Destination,
    Value,
    Data,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicSendKind {
    Transfer,
    ContractCall,
    Deploy,
}

impl PublicSendKind {
    pub(in crate::root) const fn is_advanced(self) -> bool {
        !matches!(self, Self::Transfer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct AdvancedPublicSendValidationError {
    pub(in crate::root) field: AdvancedPublicSendField,
    pub(in crate::root) message: String,
}

pub(in crate::root) fn parse_advanced_public_send_intent(
    kind: PublicSendKind,
    destination: &str,
    value: &str,
    data: &str,
    native_decimals: Option<u8>,
) -> Result<PublicTransactionIntent, AdvancedPublicSendValidationError> {
    let to = match kind {
        PublicSendKind::Transfer => {
            return Err(AdvancedPublicSendValidationError {
                field: AdvancedPublicSendField::Destination,
                message: "Choose Contract call or Deploy.".to_string(),
            });
        }
        PublicSendKind::ContractCall => {
            let destination = destination.trim();
            if destination.is_empty() {
                return Err(AdvancedPublicSendValidationError {
                    field: AdvancedPublicSendField::Destination,
                    message: "Enter a destination address.".to_string(),
                });
            }
            Some(
                parse_address(destination).ok_or_else(|| AdvancedPublicSendValidationError {
                    field: AdvancedPublicSendField::Destination,
                    message: "Enter a valid EVM destination address.".to_string(),
                })?,
            )
        }
        PublicSendKind::Deploy => None,
    };
    let value = if value.trim().is_empty() {
        U256::ZERO
    } else {
        parse_send_amount(value, native_decimals).map_err(|error| {
            AdvancedPublicSendValidationError {
                field: AdvancedPublicSendField::Value,
                message: error.to_string(),
            }
        })?
    };
    let data = data.trim();
    let data = data
        .strip_prefix("0x")
        .or_else(|| data.strip_prefix("0X"))
        .unwrap_or(data);
    let data_label = if kind == PublicSendKind::Deploy {
        "Init code"
    } else {
        "Calldata"
    };
    if !data.len().is_multiple_of(2) {
        return Err(AdvancedPublicSendValidationError {
            field: AdvancedPublicSendField::Data,
            message: format!("{data_label} must contain an even number of hexadecimal characters."),
        });
    }
    let data = alloy::hex::decode(data).map(Bytes::from).map_err(|_| {
        AdvancedPublicSendValidationError {
            field: AdvancedPublicSendField::Data,
            message: format!("{data_label} must contain only hexadecimal characters."),
        }
    })?;
    if kind == PublicSendKind::Deploy && data.is_empty() {
        return Err(AdvancedPublicSendValidationError {
            field: AdvancedPublicSendField::Data,
            message: "Enter the contract init code.".to_string(),
        });
    }
    if kind == PublicSendKind::ContractCall && value.is_zero() && data.is_empty() {
        return Err(AdvancedPublicSendValidationError {
            field: AdvancedPublicSendField::Data,
            message: "Enter calldata or a native value.".to_string(),
        });
    }
    Ok(PublicTransactionIntent::Raw { to, value, data })
}

pub(in crate::root) const fn advanced_public_send_estimate_required_message(
    invalidated: bool,
) -> &'static str {
    if invalidated {
        "The request changed. Estimate gas again before authorizing."
    } else {
        "Estimate gas before you can authorize this transaction."
    }
}

pub(in crate::root) fn authorized_public_action_gas_fee_selection(
    selection: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
    profile: PublicShieldTransactionProfile,
    chain_id: u64,
) -> Result<PublicActionGasFeeSelection, String> {
    match selection {
        PublicActionGasFeeSelection::Auto => {
            let quote = quote.ok_or_else(|| "Wait for the gas fee quote".to_string())?;
            let (max_fee_per_gas, max_priority_fee_per_gas) =
                if profile.uses_legacy_envelope(chain_id) {
                    (quote.rpc_gas_price, 0)
                } else {
                    (
                        quote.suggested_max_fee_per_gas,
                        quote.suggested_max_priority_fee_per_gas,
                    )
                };
            Ok(PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            })
        }
        custom @ PublicActionGasFeeSelection::Custom { .. } => Ok(custom),
    }
}

pub(in crate::root) fn public_action_uses_railway_authorization_ceiling(
    mode: PublicActionMode,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_fee_mode: PublicActionGasFeeMode,
) -> bool {
    mode == PublicActionMode::Shield
        && profile == PublicShieldTransactionProfile::Railway
        && matches!(asset, PublicAssetId::Erc20(_))
        && gas_fee_mode == PublicActionGasFeeMode::Auto
}

#[derive(Clone)]
pub(in crate::root) struct PublicShieldDraft {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) asset: PublicAssetId,
    pub(in crate::root) asset_label: String,
    pub(in crate::root) asset_icon_path: Option<WalletIconSource>,
    pub(in crate::root) asset_decimals: Option<u8>,
    pub(in crate::root) public_account_uuid: Arc<str>,
    pub(in crate::root) public_account_label: String,
    pub(in crate::root) public_account_source: PublicAccountSource,
    pub(in crate::root) view_session: Arc<DesktopViewSession>,
    pub(in crate::root) vault_store: Arc<DesktopVaultStore>,
    pub(in crate::root) amount: U256,
    pub(in crate::root) profile: PublicShieldTransactionProfile,
    pub(in crate::root) gas_fee: PublicActionGasFeeSelection,
    pub(in crate::root) gas_fee_mode: PublicActionGasFeeMode,
    pub(in crate::root) authorized_fee_ceiling: PublicActionGasFeeSelection,
    pub(in crate::root) fee_display: PublicActionFeeDisplay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicActionFeeDisplay {
    pub(in crate::root) gas_limit: Option<u64>,
    pub(in crate::root) expected_gas_cost: Option<String>,
    pub(in crate::root) maximum_gas_cost: Option<String>,
    pub(in crate::root) show_maximum_gas_cost: bool,
    pub(in crate::root) protocol_fee: Option<String>,
}

impl PublicActionFeeDisplay {
    pub(in crate::root) fn visible_maximum_gas_cost(&self) -> Option<&str> {
        self.show_maximum_gas_cost
            .then_some(self.maximum_gas_cost.as_deref())
            .flatten()
    }
}

pub(in crate::root) fn public_send_authorization_summary(
    draft: &PublicSendDraft,
) -> SpendAuthorizationSummary {
    if let Some(metadata) = advanced_public_send_review_metadata(&draft.intent) {
        let payload_label = if metadata.action_type == "Contract creation" {
            "init code"
        } else {
            "calldata"
        };
        let full_data = metadata.full_data.clone();
        let estimate = draft
            .advanced_estimate
            .as_ref()
            .expect("advanced Public Send draft has an estimate");
        let mut rows = vec![
            SpendAuthorizationSummaryRow::new("Type", metadata.action_type),
            SpendAuthorizationSummaryRow::new("From", draft.public_account_label.clone()),
            SpendAuthorizationSummaryRow::new("Destination", metadata.destination),
            SpendAuthorizationSummaryRow::new(
                "Native value",
                format!(
                    "{} {}",
                    format_send_amount_input(metadata.value, draft.asset_decimals),
                    draft.asset_label
                ),
            )
            .with_icon(draft.asset_icon_path.clone()),
            SpendAuthorizationSummaryRow::new(
                "Data length",
                format_advanced_data_length(metadata.data_length),
            ),
        ];
        if let Some(selector) = metadata.selector {
            rows.push(SpendAuthorizationSummaryRow::new("Selector", selector));
        }
        rows.extend([
            SpendAuthorizationSummaryRow::new("Data hash", metadata.data_hash),
            SpendAuthorizationSummaryRow::new("Gas limit", format_gas_limit(estimate.gas_limit)),
            SpendAuthorizationSummaryRow::new(
                "Expected gas cost",
                draft
                    .fee_display
                    .expected_gas_cost
                    .clone()
                    .expect("advanced Public Send draft has an expected gas-cost display"),
            ),
        ]);
        if let Some(maximum_gas_cost) = draft.fee_display.visible_maximum_gas_cost() {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Maximum gas cost",
                maximum_gas_cost,
            ));
        }
        return SpendAuthorizationSummary::new(
            "Advanced public transaction",
            public_send_authorization_detail(draft.public_account_source),
            rows,
        )
        .with_warnings(advanced_public_send_warnings(draft.public_account_source))
        .with_payload(payload_label, full_data)
        .requiring_explicit_review();
    }
    let mut rows = vec![
        SpendAuthorizationSummaryRow::new("Amount", public_action_amount_label(draft))
            .with_icon(draft.asset_icon_path.clone()),
        SpendAuthorizationSummaryRow::new("From", draft.public_account_label.clone()),
        SpendAuthorizationSummaryRow::new("Recipient", draft.recipient.to_checksum(None))
            .with_shortened_copyable(),
    ];
    rows.extend(public_action_authorization_fee_rows(&draft.fee_display));
    SpendAuthorizationSummary::new(
        "Public send",
        public_send_authorization_detail(draft.public_account_source),
        rows,
    )
}

pub(in crate::root) fn format_advanced_data_length(data_length: usize) -> String {
    let unit = if data_length == 1 { "byte" } else { "bytes" };
    format!("{data_length} {unit}")
}

pub(in crate::root) fn format_gas_limit(gas_limit: u64) -> String {
    let mut formatted = gas_limit.to_string();
    let mut separator_index = formatted.len().saturating_sub(3);
    while separator_index > 0 {
        formatted.insert(separator_index, ',');
        separator_index = separator_index.saturating_sub(3);
    }
    formatted
}

pub(in crate::root) fn advanced_public_send_warnings(source: PublicAccountSource) -> Vec<Arc<str>> {
    let mut warnings = vec![Arc::from(
        "Arbitrary transaction data can transfer assets, grant allowances, or execute unknown code. Verify the destination and full payload independently.",
    )];
    if source == PublicAccountSource::HardwareDerived {
        warnings.push(Arc::from(
            "Your hardware wallet may require blind signing and may not display or decode the complete transaction data.",
        ));
    }
    warnings
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct AdvancedPublicSendReviewMetadata {
    pub(in crate::root) action_type: &'static str,
    pub(in crate::root) destination: String,
    pub(in crate::root) value: U256,
    pub(in crate::root) data_length: usize,
    pub(in crate::root) selector: Option<String>,
    pub(in crate::root) data_hash: String,
    pub(in crate::root) full_data: String,
}

pub(in crate::root) fn advanced_public_send_review_metadata(
    intent: &PublicTransactionIntent,
) -> Option<AdvancedPublicSendReviewMetadata> {
    let PublicTransactionIntent::Raw { to, value, data } = intent else {
        return None;
    };
    let selector =
        (to.is_some() && data.len() >= 4).then(|| alloy::hex::encode_prefixed(&data[..4]));
    Some(AdvancedPublicSendReviewMetadata {
        action_type: if to.is_some() {
            "Contract call"
        } else {
            "Contract creation"
        },
        destination: to.map_or_else(|| "New contract".to_string(), |to| to.to_checksum(None)),
        value: *value,
        data_length: data.len(),
        selector,
        data_hash: alloy::hex::encode_prefixed(keccak256(data)),
        full_data: alloy::hex::encode_prefixed(data),
    })
}

pub(in crate::root) fn public_shield_authorization_summary(
    draft: &PublicShieldDraft,
) -> SpendAuthorizationSummary {
    let mut rows = Vec::new();
    if let Some(gas_limit) = draft.fee_display.gas_limit {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Gas limit",
            format_gas_limit(gas_limit),
        ));
    }
    rows.extend([
        SpendAuthorizationSummaryRow::new("Amount", public_shield_amount_label(draft))
            .with_icon(draft.asset_icon_path.clone()),
        SpendAuthorizationSummaryRow::new("From", draft.public_account_label.clone()),
        SpendAuthorizationSummaryRow::new("Recipient", "Selected private wallet"),
        SpendAuthorizationSummaryRow::new("Profile", draft.profile.display_name()),
    ]);
    rows.extend(public_action_authorization_fee_rows(&draft.fee_display));
    SpendAuthorizationSummary::new(
        "Public shield",
        public_shield_authorization_detail(draft.public_account_source),
        rows,
    )
}

fn public_action_authorization_fee_rows(
    display: &PublicActionFeeDisplay,
) -> Vec<SpendAuthorizationSummaryRow> {
    let mut rows = vec![SpendAuthorizationSummaryRow::new(
        "Expected gas cost",
        display
            .expected_gas_cost
            .as_deref()
            .unwrap_or("Unavailable"),
    )];
    if let Some(maximum_gas_cost) = display.visible_maximum_gas_cost() {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Maximum gas cost",
            maximum_gas_cost,
        ));
    }
    if let Some(protocol_fee) = display.protocol_fee.as_deref() {
        rows.push(SpendAuthorizationSummaryRow::new(
            public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS),
            protocol_fee,
        ));
    }
    rows
}

pub(in crate::root) const fn public_send_authorization_detail(
    source: PublicAccountSource,
) -> &'static str {
    match source {
        PublicAccountSource::HardwareDerived => {
            "Connect your hardware wallet and approve the public send transaction on the device. No EVM private key is stored in the vault."
        }
        PublicAccountSource::Derived | PublicAccountSource::Imported => {
            "Enter your vault password to authorize this public send."
        }
    }
}

pub(in crate::root) const fn public_shield_authorization_detail(
    source: PublicAccountSource,
) -> &'static str {
    match source {
        PublicAccountSource::HardwareDerived => {
            "Connect your hardware wallet and approve the shield key message plus public shield transactions on the device. No EVM private key is stored in the vault."
        }
        PublicAccountSource::Derived | PublicAccountSource::Imported => {
            "Enter your vault password to authorize this public shield."
        }
    }
}

pub(in crate::root) fn public_action_amount_label(draft: &PublicSendDraft) -> String {
    format!(
        "{} {}",
        format_send_amount_input(draft.amount, draft.asset_decimals),
        draft.asset_label
    )
}

pub(in crate::root) fn public_shield_amount_label(draft: &PublicShieldDraft) -> String {
    format!(
        "{} {}",
        format_send_amount_input(draft.amount, draft.asset_decimals),
        draft.asset_label
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicActionMode {
    Shield,
    Send,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicActionStepStatus {
    NotStarted,
    Pending,
    Done,
    Error,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicActionProgressLifecycle {
    Clear,
    Active,
    DialogReplacedWhileConfirmedHistoryRemains,
}

pub(in crate::root) const fn public_action_progress_handoff_lifecycle(
    confirmed_history_remains: bool,
) -> Option<PublicActionProgressLifecycle> {
    if confirmed_history_remains {
        Some(PublicActionProgressLifecycle::DialogReplacedWhileConfirmedHistoryRemains)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicActionStepInterval {
    pub(in crate::root) start: U256,
    pub(in crate::root) end: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicActionStepState {
    pub(in crate::root) step: PublicActionProgressStep,
    pub(in crate::root) status: PublicActionStepStatus,
    pub(in crate::root) tx_hash: Option<Arc<str>>,
    pub(in crate::root) message: Option<Arc<str>>,
    pub(in crate::root) interval: Option<PublicActionStepInterval>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicActionFeeAuthorizationReview {
    pub(in crate::root) step: PublicActionProgressStep,
    pub(in crate::root) max_fee_per_gas: u128,
    pub(in crate::root) max_priority_fee_per_gas: u128,
    pub(in crate::root) message: Arc<str>,
}

pub(in crate::root) fn public_action_fee_authorization_review(
    step: PublicActionProgressStep,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    message: String,
) -> PublicActionFeeAuthorizationReview {
    PublicActionFeeAuthorizationReview {
        step,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        message: Arc::from(message),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum ProgressFooterAction {
    Stop,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum ProgressDialogCloseBehavior {
    TopOnly,
    TopAndClear,
    AllAndClear,
}

pub(in crate::root) const STOPPED_PROGRESS_MESSAGE: &str =
    "Stopped locally. Already-submitted network work may continue.";

pub(in crate::root) const fn progress_footer_action(
    stop_available: bool,
    terminal: bool,
) -> ProgressFooterAction {
    if stop_available && !terminal {
        ProgressFooterAction::Stop
    } else {
        ProgressFooterAction::Close
    }
}

pub(in crate::root) const fn progress_dialog_close_behavior(
    successful: bool,
    stopped: bool,
) -> ProgressDialogCloseBehavior {
    if successful {
        ProgressDialogCloseBehavior::AllAndClear
    } else if stopped {
        ProgressDialogCloseBehavior::TopAndClear
    } else {
        ProgressDialogCloseBehavior::TopOnly
    }
}

pub(in crate::root) const fn public_action_step_is_final_handoff(
    mode: PublicActionMode,
    step: PublicActionProgressStep,
) -> bool {
    matches!(
        step,
        PublicActionProgressStep::Sponsor
            | PublicActionProgressStep::Unsponsor
            | PublicActionProgressStep::CallVote
            | PublicActionProgressStep::Vote
            | PublicActionProgressStep::Stake
            | PublicActionProgressStep::Delegate
            | PublicActionProgressStep::Unlock
            | PublicActionProgressStep::PrincipalClaim
            | PublicActionProgressStep::RewardClaim(_)
    ) || match mode {
        PublicActionMode::Shield => matches!(step, PublicActionProgressStep::Shield),
        PublicActionMode::Send => matches!(step, PublicActionProgressStep::Send),
    }
}

pub(in crate::root) fn public_action_step_is_final_handoff_for_steps(
    mode: PublicActionMode,
    step: PublicActionProgressStep,
    steps: &[PublicActionStepState],
) -> bool {
    steps.last().map_or_else(
        || public_action_step_is_final_handoff(mode, step),
        |last| last.step == step,
    )
}

pub(in crate::root) const fn public_action_accepts_update(
    current_generation: u64,
    update_generation: u64,
    stopped: bool,
) -> bool {
    current_generation == update_generation && !stopped
}

pub(in crate::root) fn public_action_progress_footer_action(
    stop_available: bool,
    steps: &[PublicActionStepState],
) -> ProgressFooterAction {
    progress_footer_action(stop_available, public_action_progress_is_terminal(steps))
}

pub(in crate::root) fn public_action_progress_is_terminal(steps: &[PublicActionStepState]) -> bool {
    !steps.is_empty()
        && (steps
            .iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
            || steps.iter().any(|step| {
                matches!(
                    step.status,
                    PublicActionStepStatus::Error | PublicActionStepStatus::Stopped
                )
            }))
}

pub(in crate::root) fn public_action_progress_is_successful(
    steps: &[PublicActionStepState],
) -> bool {
    !steps.is_empty()
        && steps
            .iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
}

pub(in crate::root) const fn public_action_discard_attempt_available(
    command_available: bool,
    step: &PublicActionStepState,
) -> bool {
    command_available && matches!(step.status, PublicActionStepStatus::Error)
}

pub(in crate::root) fn mark_public_action_active_step_stopped(
    steps: &mut [PublicActionStepState],
) -> bool {
    let step_index = steps
        .iter()
        .position(|step| step.status == PublicActionStepStatus::Pending)
        .or_else(|| {
            steps
                .iter()
                .position(|step| step.status == PublicActionStepStatus::Error)
        })
        .or_else(|| {
            steps
                .iter()
                .rposition(|step| step.status == PublicActionStepStatus::NotStarted)
        });
    let Some(step_index) = step_index else {
        return false;
    };
    let step = &mut steps[step_index];
    step.status = PublicActionStepStatus::Stopped;
    step.message = Some(Arc::from(STOPPED_PROGRESS_MESSAGE));
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicActionGasRetryKind {
    RetryStep,
    RetryEstimate,
    SpeedUp,
    FeeAuthorization,
}

pub(in crate::root) struct PublicActionGasRetryDialogContent {
    pub(in crate::root) root: Entity<WalletRoot>,
    pub(in crate::root) generation: u64,
    pub(in crate::root) retry_kind: PublicActionGasRetryKind,
    pub(in crate::root) gas_inputs: GasRetryInputs,
    pub(in crate::root) error: Option<Arc<str>>,
}

impl PublicActionGasRetryDialogContent {
    pub(in crate::root) fn new(
        root: Entity<WalletRoot>,
        generation: u64,
        retry_kind: PublicActionGasRetryKind,
        initial_max_fee_per_gas: u128,
        initial_max_priority_fee_per_gas: u128,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let gas_inputs = GasRetryInputs::new(
            initial_max_fee_per_gas,
            initial_max_priority_fee_per_gas,
            window,
            cx,
        );
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        gas_inputs.subscribe_clear_error(cx, |this, cx| {
            this.error = None;
            cx.notify();
        });
        Self {
            root,
            generation,
            retry_kind,
            gas_inputs,
            error: None,
        }
    }
}

impl gpui::Render for PublicActionGasRetryDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let title = match self.retry_kind {
            PublicActionGasRetryKind::RetryStep => "Retry step",
            PublicActionGasRetryKind::RetryEstimate => "Retry with custom gas",
            PublicActionGasRetryKind::SpeedUp => "Speed up transaction",
            PublicActionGasRetryKind::FeeAuthorization => "Authorize and continue",
        };
        let detail = match self.retry_kind {
            PublicActionGasRetryKind::RetryStep => {
                "Retry this Public action step using the current gas fee values."
            }
            PublicActionGasRetryKind::RetryEstimate => {
                "Retry this Public action step using these EIP-1559 fee values."
            }
            PublicActionGasRetryKind::SpeedUp => {
                "Uses the same nonce to replace the pending transaction. Values are prefilled +12.5%."
            }
            PublicActionGasRetryKind::FeeAuthorization => {
                "Network fees changed. Review the updated Railway Standard fee to continue."
            }
        };
        let submit_root = self.root.clone();
        let gas_inputs = self.gas_inputs.clone();
        let generation = self.generation;
        let retry_kind = self.retry_kind;
        let dialog = cx.entity();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_strong_text(title))
            .child(app_muted_text(detail).whitespace_normal())
            .child(self.gas_inputs.render_fields())
            .when_some(self.error.as_ref(), |this, error| {
                this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("public-action-gas-retry-cancel", "Cancel")
                            .flex_none()
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button(
                            "public-action-gas-retry-confirm",
                            if retry_kind == PublicActionGasRetryKind::FeeAuthorization {
                                "Authorize and continue"
                            } else {
                                "Submit"
                            },
                        )
                        .primary()
                        .flex_none()
                        .on_click(move |_event, window, cx| {
                            let (max_fee, max_tip) = match gas_inputs.parse(cx) {
                                Ok(values) => values,
                                Err(error) => {
                                    dialog.update(cx, |this, cx| {
                                        this.error = Some(Arc::from(error));
                                        cx.notify();
                                    });
                                    return;
                                }
                            };
                            submit_root.update(cx, |root, cx| {
                                root.submit_public_action_gas_retry(
                                    generation, retry_kind, max_fee, max_tip, cx,
                                );
                            });
                            window.close_dialog(cx);
                        }),
                    ),
            )
    }
}
