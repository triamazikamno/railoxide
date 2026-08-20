use std::sync::Arc;

use alloy::primitives::{B256, TxKind, U256};
use gpui::{Context, Focusable, Window};
use wallet_ops::{
    HttpContext, PublicActionFeeProjection, PublicActionFeeSource, PublicActionGasFeeQuote,
    PublicActionGasFeeSelection, PublicActionResolvedGasFee, PublicAdvancedTransactionEstimate,
    PublicAdvancedTransactionEstimateRequest, PublicAdvancedTransactionSimulationError,
    PublicShieldTransactionProfile, PublicTransactionIntent, SelfBroadcastGasFeeQuote,
    WalletConnectDecodedCallKind, WalletConnectParsedRequest, WalletConnectReviewedFee,
    WalletConnectReviewedTransaction, project_public_action_fee,
    public_native_action_gas_units_from_walletconnect_intent, quote_public_action_gas_fee,
    resolve_public_action_gas_fee, simulate_public_advanced_transaction_with_fee,
};

use super::helpers::{
    current_unix_seconds, parse_caip2_chain_id, walletconnect_await_before_request_expiry,
    walletconnect_duration_until_expiry, walletconnect_request_approval_admitted,
};
use super::requests::transaction_request_from_walletconnect;
use super::{WalletConnectRequestUi, WalletRoot};
use crate::root::retry::retry_backoff_delay_capped;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectFeeStatus {
    Fetching,
    EstimatedFromOperation(PublicActionFeeProjection),
    AwaitingSimulation,
    Simulated(PublicActionFeeProjection),
    UnavailableFailed,
    WouldRevert,
}

pub(super) const fn walletconnect_fee_retrying(
    retry_attempt: u8,
    refreshing: bool,
    simulation_requested: bool,
) -> bool {
    retry_attempt > 0 && (refreshing || simulation_requested)
}

pub(super) const fn walletconnect_fee_retry_action_enabled(
    status: WalletConnectFeeStatus,
    refreshing: bool,
    simulation_requested: bool,
) -> bool {
    matches!(status, WalletConnectFeeStatus::UnavailableFailed)
        && !refreshing
        && !simulation_requested
}

pub(super) const fn walletconnect_request_fee_eligible(
    request: &WalletConnectParsedRequest,
) -> bool {
    matches!(
        request,
        WalletConnectParsedRequest::EthSendTransaction { .. }
    )
}

pub(super) const fn validate_walletconnect_reviewed_fee_pairing(
    request: &WalletConnectParsedRequest,
    reviewed_fee: Option<&WalletConnectReviewedFeeProjection>,
) -> Result<(), &'static str> {
    match (
        walletconnect_request_fee_eligible(request),
        reviewed_fee.is_some(),
    ) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err("WalletConnect transaction approval is missing current fee review."),
        (false, true) => Err("WalletConnect non-transaction approval has unexpected fee review."),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectFeeState {
    pub(super) request_key: Arc<str>,
    pub(super) review_token: u64,
    pub(super) payload_fingerprint: B256,
    pub(super) request_generation: u64,
    pub(super) dialog_generation: u64,
    pub(super) editor_generation: u64,
    pub(super) expiry_timestamp: Option<u64>,
    pub(super) status: WalletConnectFeeStatus,
    pub(super) error: Option<Arc<str>>,
    pub(super) retry_attempt: u8,
    pub(super) generation: u64,
    pub(super) simulation_requested: bool,
    pub(super) simulation_retryable: bool,
    /// Display continuity only; never use this projection for authorization or submission.
    pub(super) last_successful_display_projection: Option<PublicActionFeeProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectFeeCompletionGuard {
    pub(super) request_generation: u64,
    pub(super) dialog_generation: u64,
    pub(super) editor_generation: u64,
    pub(super) generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectSimulationResult {
    Complete(PublicAdvancedTransactionEstimate),
    Error(WalletConnectSimulationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectSimulationError {
    Reverted(Arc<str>),
    Unavailable(Arc<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum WalletConnectReviewedFeeBasis {
    Unresolved,
    OperationTable,
    NetworkSimulation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct WalletConnectReviewedFeeProjection {
    pub(super) request_key: Arc<str>,
    pub(super) review_token: u64,
    pub(super) payload_fingerprint: B256,
    pub(super) editor_generation: u64,
    pub(super) selection: PublicActionGasFeeSelection,
    pub(super) selected_max_fee_per_gas: Option<u128>,
    pub(super) selected_max_priority_fee_per_gas: Option<u128>,
    pub(super) raw_gas_limit: Option<u64>,
    pub(super) gas_limit: Option<u64>,
    pub(super) source: Option<PublicActionFeeSource>,
    pub(super) expected_gas_cost: Option<U256>,
    pub(super) maximum_gas_cost: Option<U256>,
    pub(super) basis: WalletConnectReviewedFeeBasis,
}

impl WalletConnectReviewedFeeProjection {
    #[cfg(test)]
    pub(super) fn unresolved(request_key: &str, review_token: u64) -> Self {
        Self {
            request_key: Arc::from(request_key),
            review_token,
            payload_fingerprint: B256::ZERO,
            editor_generation: 0,
            selection: PublicActionGasFeeSelection::Auto,
            selected_max_fee_per_gas: None,
            selected_max_priority_fee_per_gas: None,
            raw_gas_limit: None,
            gas_limit: None,
            source: None,
            expected_gas_cost: None,
            maximum_gas_cost: None,
            basis: WalletConnectReviewedFeeBasis::Unresolved,
        }
    }

    pub(super) fn wallet_ops_fee(&self) -> Option<WalletConnectReviewedFee> {
        Some(WalletConnectReviewedFee {
            max_fee_per_gas: self.selected_max_fee_per_gas?,
            max_priority_fee_per_gas: self.selected_max_priority_fee_per_gas?,
        })
    }

    pub(super) fn reviewed_transaction(&self) -> Option<WalletConnectReviewedTransaction> {
        (self.basis == WalletConnectReviewedFeeBasis::NetworkSimulation).then_some(
            WalletConnectReviewedTransaction {
                payload_fingerprint: self.payload_fingerprint,
                gas_limit: self.gas_limit?,
            },
        )
    }
}

fn projection_matches_review(
    projection: PublicActionFeeProjection,
    reviewed: &WalletConnectReviewedFeeProjection,
) -> bool {
    Some(projection.source) == reviewed.source
        && Some(projection.raw_gas_limit) == reviewed.raw_gas_limit
        && Some(projection.gas_limit) == reviewed.gas_limit
        && Some(projection.max_fee_per_gas) == reviewed.selected_max_fee_per_gas
        && Some(projection.max_priority_fee_per_gas) == reviewed.selected_max_priority_fee_per_gas
        && Some(projection.expected_gas_cost) == reviewed.expected_gas_cost
        && Some(projection.maximum_gas_cost) == reviewed.maximum_gas_cost
}

pub(super) fn walletconnect_reviewed_fee_request_context_matches(
    reviewed: &WalletConnectReviewedFeeProjection,
    request_key: &str,
    review_token: u64,
    payload_fingerprint: B256,
) -> bool {
    reviewed.request_key.as_ref() == request_key
        && reviewed.review_token == review_token
        && reviewed.payload_fingerprint == payload_fingerprint
}

pub(super) fn walletconnect_reviewed_fee_editor_is_current(
    reviewed: &WalletConnectReviewedFeeProjection,
    editor_generation: u64,
    selection: PublicActionGasFeeSelection,
) -> bool {
    reviewed.editor_generation == editor_generation && reviewed.selection == selection
}

impl WalletConnectFeeState {
    pub(super) fn new(
        request: &WalletConnectRequestUi,
        payload_fingerprint: B256,
        request_generation: u64,
        dialog_generation: u64,
        editor_generation: u64,
    ) -> Self {
        Self {
            request_key: Arc::from(request.key.as_str()),
            review_token: request.review_token,
            payload_fingerprint,
            request_generation,
            dialog_generation,
            editor_generation,
            expiry_timestamp: request.item.expiry_timestamp,
            status: WalletConnectFeeStatus::Fetching,
            error: None,
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        }
    }

    pub(super) const fn completion_guard(&self) -> WalletConnectFeeCompletionGuard {
        WalletConnectFeeCompletionGuard {
            request_generation: self.request_generation,
            dialog_generation: self.dialog_generation,
            editor_generation: self.editor_generation,
            generation: self.generation,
        }
    }

    pub(super) fn is_completion_current(
        &self,
        request_key: &str,
        review_token: u64,
        payload_fingerprint: B256,
        guard: WalletConnectFeeCompletionGuard,
        open: bool,
        now: u64,
    ) -> bool {
        open && self.request_key.as_ref() == request_key
            && self.review_token == review_token
            && self.payload_fingerprint == payload_fingerprint
            && self.request_generation == guard.request_generation
            && self.dialog_generation == guard.dialog_generation
            && self.editor_generation == guard.editor_generation
            && self.generation == guard.generation
            && self.expiry_timestamp.is_none_or(|expiry| expiry > now)
    }

    pub(super) const fn begin_attempt(
        &mut self,
        editor_generation: u64,
    ) -> WalletConnectFeeCompletionGuard {
        self.editor_generation = editor_generation;
        self.generation = self.generation.wrapping_add(1);
        self.completion_guard()
    }

    pub(super) const fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.simulation_requested = false;
        self.simulation_retryable = false;
        self.last_successful_display_projection = None;
    }

    pub(super) const fn begin_manual_retry(&mut self) {
        self.retry_attempt = self.retry_attempt.saturating_add(1);
    }

    pub(super) const fn authoritative_projection(&self) -> Option<PublicActionFeeProjection> {
        match self.status {
            WalletConnectFeeStatus::EstimatedFromOperation(projection)
            | WalletConnectFeeStatus::Simulated(projection) => Some(projection),
            WalletConnectFeeStatus::Fetching
            | WalletConnectFeeStatus::AwaitingSimulation
            | WalletConnectFeeStatus::UnavailableFailed
            | WalletConnectFeeStatus::WouldRevert => None,
        }
    }

    pub(super) const fn apply_successful_operation_projection(
        &mut self,
        projection: PublicActionFeeProjection,
    ) {
        self.last_successful_display_projection = Some(projection);
        self.status = WalletConnectFeeStatus::EstimatedFromOperation(projection);
    }

    pub(super) const fn apply_successful_simulation_projection(
        &mut self,
        projection: PublicActionFeeProjection,
    ) {
        self.last_successful_display_projection = Some(projection);
        self.status = WalletConnectFeeStatus::Simulated(projection);
    }
}

pub(super) const fn walletconnect_fee_state_projection(
    state: &WalletConnectFeeState,
    refreshing: bool,
) -> Option<PublicActionFeeProjection> {
    match state.status {
        WalletConnectFeeStatus::EstimatedFromOperation(projection)
        | WalletConnectFeeStatus::Simulated(projection) => Some(projection),
        WalletConnectFeeStatus::Fetching
        | WalletConnectFeeStatus::AwaitingSimulation
        | WalletConnectFeeStatus::UnavailableFailed
        | WalletConnectFeeStatus::WouldRevert
            if refreshing || state.simulation_requested =>
        {
            state.last_successful_display_projection
        }
        WalletConnectFeeStatus::Fetching
        | WalletConnectFeeStatus::AwaitingSimulation
        | WalletConnectFeeStatus::UnavailableFailed
        | WalletConnectFeeStatus::WouldRevert => None,
    }
}

pub(super) fn walletconnect_request_payload_fingerprint(
    request: &WalletConnectRequestUi,
) -> Result<B256, String> {
    let WalletConnectParsedRequest::EthSendTransaction { transaction } = &request.parsed else {
        return Err("WalletConnect request is not a transaction".to_owned());
    };
    let chain_id = parse_chain_id(&request.item.chain_id)?;
    let tx_req = transaction_request_from_walletconnect(chain_id, transaction.clone())
        .map_err(|error| error.to_string())?;
    let tx_req =
        wallet_ops::sanitize_walletconnect_transaction_request(tx_req, chain_id, transaction.from);
    Ok(wallet_ops::walletconnect_transaction_payload_fingerprint(
        chain_id,
        transaction.from,
        &tx_req,
    ))
}

pub(super) fn walletconnect_request_raw_gas(request: &WalletConnectRequestUi) -> Option<u64> {
    request
        .item
        .decoded_transaction
        .as_ref()
        .and_then(|decoded| public_native_action_gas_units_from_walletconnect_intent(&decoded.kind))
}

pub(super) fn walletconnect_request_can_simulate(request: &WalletConnectRequestUi) -> bool {
    let WalletConnectParsedRequest::EthSendTransaction { .. } = &request.parsed else {
        return false;
    };
    request
        .item
        .decoded_transaction
        .as_ref()
        .is_none_or(|decoded| {
            matches!(
                decoded.kind,
                WalletConnectDecodedCallKind::ContractCall { .. }
                    | WalletConnectDecodedCallKind::ContractCreation
            )
        })
}

pub(super) fn walletconnect_fee_projection(
    chain_id: u64,
    raw_gas_limit: u64,
    gas_limit_buffer: u64,
    quote: Option<PublicActionGasFeeQuote>,
    selection: PublicActionGasFeeSelection,
    native_usd_micro_rate: Option<U256>,
) -> Result<(PublicActionFeeProjection, PublicActionResolvedGasFee), String> {
    let resolved = resolve_public_action_gas_fee(
        chain_id,
        PublicShieldTransactionProfile::Railoxide,
        selection,
        quote,
    )
    .map_err(|error| error.to_string())?;
    let projection_quote = quote
        .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(resolved.max_fee_per_gas));
    Ok((
        project_public_action_fee(
            raw_gas_limit,
            raw_gas_limit.saturating_add(gas_limit_buffer),
            projection_quote,
            resolved,
            PublicActionFeeSource::OperationTable,
            native_usd_micro_rate,
        ),
        resolved,
    ))
}

pub(super) fn walletconnect_transaction_estimate_request(
    request: &WalletConnectRequestUi,
    chain_id: u64,
    effective_chain: Option<wallet_ops::settings::EffectiveChainConfig>,
) -> Result<PublicAdvancedTransactionEstimateRequest, String> {
    let WalletConnectParsedRequest::EthSendTransaction { transaction } = &request.parsed else {
        return Err("WalletConnect request is not a transaction".to_owned());
    };
    let tx_req = transaction_request_from_walletconnect(chain_id, transaction.clone())
        .map_err(|error| error.to_string())?;
    let to = match tx_req.to {
        Some(TxKind::Call(to)) => Some(to),
        Some(TxKind::Create) | None => None,
    };
    let data = tx_req.input.input().cloned().unwrap_or_default();
    Ok(PublicAdvancedTransactionEstimateRequest {
        chain_id,
        effective_chain,
        from: transaction.from,
        intent: PublicTransactionIntent::Raw {
            to,
            value: tx_req.value.unwrap_or(U256::ZERO),
            data,
        },
        gas_fee: PublicActionGasFeeSelection::Auto,
        access_list: tx_req.access_list,
    })
}

pub(super) async fn quote_walletconnect_fee_with_retry(
    chain_id: u64,
    effective_chain: Option<wallet_ops::settings::EffectiveChainConfig>,
    http: HttpContext,
    expiry_timestamp: Option<u64>,
) -> Result<PublicActionGasFeeQuote, String> {
    const MAX_ATTEMPTS_WITHOUT_EXPIRY: u8 = 4;
    let mut attempt = 0;
    let network_mode = http.network_mode();
    loop {
        let remaining = expiry_timestamp.and_then(walletconnect_duration_until_expiry);
        if expiry_timestamp.is_some() && remaining.is_none() {
            tracing::debug!(
                chain_id,
                network_mode = %network_mode,
                attempt,
                "WalletConnect fee quote exhausted at request expiry"
            );
            return Err("WalletConnect request expired before fee quote completed".to_owned());
        }
        tracing::debug!(
            chain_id,
            network_mode = %network_mode,
            attempt,
            "WalletConnect fee quote attempt started"
        );
        let result = match remaining {
            Some(remaining) => {
                if let Ok(result) = tokio::time::timeout(
                    remaining,
                    quote_public_action_gas_fee(chain_id, effective_chain.as_ref(), &http),
                )
                .await
                {
                    result
                } else {
                    tracing::debug!(
                        chain_id,
                        network_mode = %network_mode,
                        attempt,
                        "WalletConnect fee quote exhausted at request expiry"
                    );
                    return Err("WalletConnect request expired while fetching fee quote".to_owned());
                }
            }
            None => quote_public_action_gas_fee(chain_id, effective_chain.as_ref(), &http).await,
        };
        match result {
            Ok(quote) => {
                tracing::debug!(
                    chain_id,
                    network_mode = %network_mode,
                    attempt,
                    "WalletConnect fee quote attempt succeeded"
                );
                return Ok(quote);
            }
            Err(error)
                if expiry_timestamp.is_none() && attempt + 1 >= MAX_ATTEMPTS_WITHOUT_EXPIRY =>
            {
                tracing::debug!(
                    chain_id,
                    network_mode = %network_mode,
                    attempt,
                    "WalletConnect fee quote retries exhausted"
                );
                return Err(error.to_string());
            }
            Err(_error) => {
                let remaining = expiry_timestamp.and_then(walletconnect_duration_until_expiry);
                let Some(wait) = retry_backoff_delay_capped(attempt, remaining) else {
                    tracing::debug!(
                        chain_id,
                        network_mode = %network_mode,
                        attempt,
                        "WalletConnect fee quote retries exhausted at request expiry"
                    );
                    return Err("WalletConnect request expired before fee retry".to_owned());
                };
                tracing::debug!(
                    chain_id,
                    network_mode = %network_mode,
                    attempt,
                    delay_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    "WalletConnect fee quote failed; backing off before retry"
                );
                tokio::time::sleep(wait).await;
                if expiry_timestamp.is_some()
                    && walletconnect_duration_until_expiry(expiry_timestamp.unwrap()).is_none()
                {
                    tracing::debug!(
                        chain_id,
                        network_mode = %network_mode,
                        attempt,
                        "WalletConnect fee quote retries exhausted at request expiry"
                    );
                    return Err("WalletConnect request expired before fee retry".to_owned());
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

pub(super) async fn simulate_walletconnect_transaction(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
) -> WalletConnectSimulationResult {
    match simulate_public_advanced_transaction_with_fee(request, quote, resolved, http).await {
        Ok(estimate) => WalletConnectSimulationResult::Complete(estimate),
        Err(PublicAdvancedTransactionSimulationError::Reverted(error)) => {
            WalletConnectSimulationResult::Error(WalletConnectSimulationError::Reverted(Arc::from(
                error,
            )))
        }
        Err(PublicAdvancedTransactionSimulationError::Unavailable(error)) => {
            WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(
                Arc::from(error),
            ))
        }
    }
}

impl WalletRoot {
    pub(super) fn capture_walletconnect_reviewed_fee(
        &self,
        request: &WalletConnectRequestUi,
        cx: &Context<'_, Self>,
    ) -> Result<WalletConnectReviewedFeeProjection, String> {
        let payload_fingerprint = walletconnect_request_payload_fingerprint(request)?;
        let selection = self.walletconnect.walletconnect_gas_fee.selection(cx)?;
        let state = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| state.request_key.as_ref() == request.key);
        let state_projection = state.and_then(WalletConnectFeeState::authoritative_projection);
        let chain_id = parse_chain_id(&request.item.chain_id)?;
        let raw_gas_limit = walletconnect_request_raw_gas(request);
        let gas_limit_buffer = self
            .effective_chain_configs
            .get(&chain_id)
            .map_or(0, |chain| chain.gas.gas_limit_buffer);
        let projection = state_projection.or_else(|| {
            let raw_gas_limit = raw_gas_limit?;
            let quote = self.walletconnect.walletconnect_gas_fee.quote;
            walletconnect_fee_projection(
                chain_id,
                raw_gas_limit,
                gas_limit_buffer,
                quote,
                selection,
                self.public_broadcaster_anchor_cache
                    .cached_native_usd_rate(chain_id),
            )
            .ok()
            .map(|(projection, _)| projection)
        });
        let basis = projection.map_or(WalletConnectReviewedFeeBasis::Unresolved, |projection| {
            match projection.source {
                PublicActionFeeSource::OperationTable => {
                    WalletConnectReviewedFeeBasis::OperationTable
                }
                PublicActionFeeSource::NetworkSimulation => {
                    WalletConnectReviewedFeeBasis::NetworkSimulation
                }
            }
        });
        let (selected_max_fee_per_gas, selected_max_priority_fee_per_gas) = projection.map_or_else(
            || match selection {
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                } => (Some(max_fee_per_gas), Some(max_priority_fee_per_gas)),
                PublicActionGasFeeSelection::Auto => (None, None),
            },
            |projection| {
                (
                    Some(projection.max_fee_per_gas),
                    Some(projection.max_priority_fee_per_gas),
                )
            },
        );
        Ok(WalletConnectReviewedFeeProjection {
            request_key: Arc::from(request.key.as_str()),
            review_token: request.review_token,
            payload_fingerprint,
            editor_generation: self.walletconnect.walletconnect_gas_fee.refresh_id,
            selection,
            selected_max_fee_per_gas,
            selected_max_priority_fee_per_gas,
            raw_gas_limit: projection
                .map(|projection| projection.raw_gas_limit)
                .or(raw_gas_limit),
            gas_limit: projection.map(|projection| projection.gas_limit),
            source: projection.map(|projection| projection.source),
            expected_gas_cost: projection.map(|projection| projection.expected_gas_cost),
            maximum_gas_cost: projection.map(|projection| projection.maximum_gas_cost),
            basis,
        })
    }

    pub(super) fn validate_walletconnect_reviewed_fee(
        &self,
        request: &WalletConnectRequestUi,
        reviewed: &WalletConnectReviewedFeeProjection,
        cx: &Context<'_, Self>,
    ) -> Result<(), String> {
        if reviewed.request_key.as_ref() != request.key
            || reviewed.review_token != request.review_token
            || !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request.key.as_str())
            || !walletconnect_request_approval_admitted(
                request.item.expiry_timestamp,
                current_unix_seconds(),
            )
        {
            return Err(
                "WalletConnect fee review is stale; review the current request again.".to_owned(),
            );
        }
        let fingerprint = walletconnect_request_payload_fingerprint(request)?;
        if !walletconnect_reviewed_fee_request_context_matches(
            reviewed,
            request.key.as_str(),
            request.review_token,
            fingerprint,
        ) {
            return Err(
                "WalletConnect transaction changed after fee review; review it again.".to_owned(),
            );
        }
        let selection = self.walletconnect.walletconnect_gas_fee.selection(cx)?;
        if !walletconnect_reviewed_fee_editor_is_current(
            reviewed,
            self.walletconnect.walletconnect_gas_fee.refresh_id,
            selection,
        ) {
            return Err(
                "WalletConnect fee selection changed after review; review it again.".to_owned(),
            );
        }
        let Some(state) = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| state.request_key.as_ref() == request.key)
        else {
            return Err("WalletConnect fee review is no longer available.".to_owned());
        };
        if state.review_token != request.review_token || state.payload_fingerprint != fingerprint {
            return Err("WalletConnect fee review is no longer current.".to_owned());
        }
        if state.editor_generation != self.walletconnect.walletconnect_gas_fee.refresh_id {
            return Err("WalletConnect fee review is no longer current.".to_owned());
        }
        if state.simulation_requested || matches!(state.status, WalletConnectFeeStatus::WouldRevert)
        {
            return Err(
                "WalletConnect simulation changed after review; review it again.".to_owned(),
            );
        }
        let current_projection = state.authoritative_projection();
        match reviewed.basis {
            WalletConnectReviewedFeeBasis::Unresolved => {
                if current_projection.is_some()
                    && matches!(
                        state.status,
                        WalletConnectFeeStatus::Simulated(_) | WalletConnectFeeStatus::WouldRevert
                    )
                {
                    return Err(
                        "WalletConnect simulation changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
            WalletConnectReviewedFeeBasis::OperationTable => {
                if !matches!(
                    state.status,
                    WalletConnectFeeStatus::EstimatedFromOperation(_)
                ) || !current_projection
                    .is_some_and(|projection| projection_matches_review(projection, reviewed))
                {
                    return Err(
                        "WalletConnect fee estimate changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
            WalletConnectReviewedFeeBasis::NetworkSimulation => {
                if !matches!(state.status, WalletConnectFeeStatus::Simulated(_))
                    || !current_projection
                        .is_some_and(|projection| projection_matches_review(projection, reviewed))
                {
                    return Err(
                        "WalletConnect simulation changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn discard_walletconnect_fee_for_request_replacement(&mut self) {
        self.walletconnect.walletconnect_fee_state = None;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect.walletconnect_gas_fee.quote = None;
        self.walletconnect.walletconnect_gas_fee.refreshing = false;
        self.walletconnect.walletconnect_gas_fee.quote_error = None;
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
    }

    pub(super) fn attach_walletconnect_fee_state(
        &mut self,
        request_key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        };
        if !walletconnect_request_fee_eligible(&request.parsed) {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        }
        let Ok(payload_fingerprint) = walletconnect_request_payload_fingerprint(&request) else {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        };
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| {
                state.request_key.as_ref() == request_key
                    && state.review_token == request.review_token
                    && state.payload_fingerprint == payload_fingerprint
            })
        {
            return;
        }

        self.walletconnect_fee_lifecycle_reset(window, cx);
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
        let request_generation = self.walletconnect.walletconnect_fee_request_generation;
        let request_key_arc = Arc::<str>::from(request_key);
        let editor_generation = self.walletconnect.walletconnect_gas_fee.refresh_id;
        self.walletconnect.walletconnect_fee_state = Some(WalletConnectFeeState::new(
            &request,
            payload_fingerprint,
            request_generation,
            self.walletconnect.request_dialog_refresh_generation,
            editor_generation,
        ));
        self.refresh_walletconnect_gas_fee_quote(request_key_arc, cx);
    }

    pub(super) fn discard_walletconnect_fee_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.walletconnect_fee_lifecycle_reset(window, cx);
        self.walletconnect.walletconnect_fee_state = None;
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
    }

    fn walletconnect_fee_lifecycle_reset(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.walletconnect
            .walletconnect_gas_fee
            .reset_for_request(window, cx);
        if let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() {
            state.invalidate();
        }
    }

    pub(in crate::root) fn set_walletconnect_gas_fee_mode(
        &mut self,
        request_key: &Arc<str>,
        mode: super::super::gas_fee::Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key.as_ref())
            || self
                .walletconnect
                .walletconnect_fee_state
                .as_ref()
                .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        if self.walletconnect.walletconnect_gas_fee.mode == mode {
            return;
        }
        if mode == super::super::gas_fee::Eip1559GasFeeMode::Custom {
            self.walletconnect
                .walletconnect_gas_fee
                .seed_custom_from_auto_if_empty(window, cx);
        }
        self.walletconnect.walletconnect_gas_fee.mode = mode;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect_fee_state_changed_by_editor(cx);
        cx.notify();
    }

    pub(in crate::root) fn customize_walletconnect_gas_fee_from_auto(
        &mut self,
        request_key: &Arc<str>,
        target: super::super::gas_fee::Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        if !self
            .walletconnect
            .walletconnect_gas_fee
            .overwrite_custom_from_auto(window, cx)
        {
            return;
        }
        self.walletconnect.walletconnect_gas_fee.mode =
            super::super::gas_fee::Eip1559GasFeeMode::Custom;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        let input = match target {
            super::super::gas_fee::Eip1559GasFeeEditTarget::MaxFee => self
                .walletconnect
                .walletconnect_gas_fee
                .max_fee_input
                .clone(),
            super::super::gas_fee::Eip1559GasFeeEditTarget::MaxTip => self
                .walletconnect
                .walletconnect_gas_fee
                .max_priority_fee_input
                .clone(),
        };
        input.read(cx).focus_handle(cx).focus(window);
        self.walletconnect_fee_state_changed_by_editor(cx);
        cx.notify();
    }

    pub(super) fn walletconnect_fee_editor_changed(&mut self, cx: &Context<'_, Self>) {
        self.walletconnect.walletconnect_gas_fee.refreshing = false;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect_fee_state_changed_by_editor(cx);
    }

    fn walletconnect_fee_state_changed_by_editor(&mut self, cx: &Context<'_, Self>) {
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.invalidate();
        state.editor_generation = self.walletconnect.walletconnect_gas_fee.refresh_id;
        state.error = None;
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(state.request_key.as_ref())
            .cloned()
        else {
            return;
        };
        let Some(raw_gas) = walletconnect_request_raw_gas(&request) else {
            state.status = WalletConnectFeeStatus::AwaitingSimulation;
            return;
        };
        let Ok(selection) = self.walletconnect.walletconnect_gas_fee.selection(cx) else {
            state.status = WalletConnectFeeStatus::UnavailableFailed;
            return;
        };
        let chain_id = match parse_chain_id(&request.item.chain_id) {
            Ok(chain_id) => chain_id,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                return;
            }
        };
        let gas_limit_buffer = self
            .effective_chain_configs
            .get(&chain_id)
            .map_or(0, |chain| chain.gas.gas_limit_buffer);
        match walletconnect_fee_projection(
            chain_id,
            raw_gas,
            gas_limit_buffer,
            self.walletconnect.walletconnect_gas_fee.quote,
            selection,
            self.public_broadcaster_anchor_cache
                .cached_native_usd_rate(chain_id),
        ) {
            Ok((projection, _)) => {
                state.apply_successful_operation_projection(projection);
            }
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
            }
        }
    }

    pub(in crate::root) fn refresh_walletconnect_gas_fee_quote(
        &mut self,
        request_key: Arc<str>,
        cx: &Context<'_, Self>,
    ) {
        if !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key.as_ref())
            || self
                .walletconnect
                .walletconnect_fee_state
                .as_ref()
                .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key.as_ref())
            .cloned()
        else {
            return;
        };
        let Ok(chain_id) = parse_chain_id(&request.item.chain_id) else {
            return;
        };
        if self.walletconnect.walletconnect_gas_fee.refreshing {
            return;
        }
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let expiry_timestamp = request.item.expiry_timestamp;
        let http = self.http.clone();
        let network_mode = http.network_mode();
        let refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect.walletconnect_gas_fee.refresh_id = refresh_id;
        self.walletconnect.walletconnect_gas_fee.refreshing = true;
        self.walletconnect.walletconnect_gas_fee.quote_error = None;
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.begin_attempt(refresh_id);
        state.status = WalletConnectFeeStatus::Fetching;
        state.error = None;
        let guard = state.completion_guard();
        let review_token = request.review_token;
        let payload_fingerprint = state.payload_fingerprint;
        let request_generation = state.request_generation;
        let retry_attempt = state.retry_attempt;
        cx.spawn(async move |this, cx| {
            let result = quote_walletconnect_fee_with_retry(
                chain_id,
                effective_chain,
                http,
                expiry_timestamp,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                let request_matches = root
                    .walletconnect
                    .pending_requests
                    .get(request_key.as_ref())
                    .is_some_and(|request| {
                        request.review_token == review_token
                            && request.item.expiry_timestamp
                                == root
                                    .walletconnect
                                    .walletconnect_fee_state
                                    .as_ref()
                                    .and_then(|state| state.expiry_timestamp)
                            && walletconnect_request_payload_fingerprint(request)
                                .is_ok_and(|fingerprint| fingerprint == payload_fingerprint)
                    });
                let current = request_matches
                    && !root
                        .walletconnect
                        .request_actions
                        .contains(request_key.as_ref())
                    && root
                        .walletconnect
                        .walletconnect_fee_state
                        .as_ref()
                        .is_some_and(|state| {
                            state.is_completion_current(
                                request_key.as_ref(),
                                review_token,
                                payload_fingerprint,
                                guard,
                                root.walletconnect.request_dialog_open
                                    && root.walletconnect.request_dialog_key.as_deref()
                                        == Some(request_key.as_ref())
                                    && root.walletconnect.walletconnect_fee_request_generation
                                        == request_generation,
                                current_unix_seconds(),
                            ) && root.walletconnect.walletconnect_gas_fee.refresh_id == refresh_id
                        });
                if !current {
                    tracing::debug!(
                        chain_id,
                        network_mode = %network_mode,
                        attempt = retry_attempt,
                        "WalletConnect fee quote completion discarded as stale"
                    );
                    return;
                }
                root.walletconnect.walletconnect_gas_fee.refreshing = false;
                match result {
                    Ok(quote) => {
                        root.walletconnect.walletconnect_gas_fee.quote = Some(quote);
                        root.walletconnect.walletconnect_gas_fee.quote_error = None;
                        let simulate_after_quote = root
                            .walletconnect
                            .walletconnect_fee_state
                            .as_ref()
                            .is_some_and(|state| state.simulation_requested);
                        root.walletconnect_fee_state_changed_by_editor(cx);
                        if simulate_after_quote {
                            root.simulate_current_walletconnect_transaction(
                                request_key.as_ref(),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        root.walletconnect.walletconnect_gas_fee.quote_error =
                            Some(Arc::from(error.clone()));
                        if let Some(state) = root.walletconnect.walletconnect_fee_state.as_mut() {
                            state.status = WalletConnectFeeStatus::UnavailableFailed;
                            state.error = Some(Arc::from(error));
                            state.simulation_requested = false;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn retry_walletconnect_fee(&mut self, request_key: &str, cx: &Context<'_, Self>) {
        let Some(state) = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| {
                state.request_key.as_ref() == request_key
                    && matches!(state.status, WalletConnectFeeStatus::UnavailableFailed)
            })
        else {
            return;
        };
        let simulation_retryable = state.simulation_retryable;
        if let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() {
            state.begin_manual_retry();
        }
        if simulation_retryable {
            self.simulate_current_walletconnect_transaction(request_key, cx);
        } else {
            self.refresh_walletconnect_gas_fee_quote(Arc::from(request_key), cx);
        }
    }

    pub(super) fn simulate_current_walletconnect_transaction(
        &mut self,
        request_key: &str,
        cx: &Context<'_, Self>,
    ) {
        if !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key)
        {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        if !walletconnect_request_can_simulate(&request)
            || !walletconnect_request_approval_admitted(
                request.item.expiry_timestamp,
                current_unix_seconds(),
            )
        {
            return;
        }
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.simulation_requested = true;
        state.simulation_retryable = true;
        let selection = match self.walletconnect.walletconnect_gas_fee.selection(cx) {
            Ok(selection) => selection,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                state.simulation_requested = false;
                return;
            }
        };
        let quote = match (selection, self.walletconnect.walletconnect_gas_fee.quote) {
            (PublicActionGasFeeSelection::Auto, Some(quote)) => Some(quote),
            (PublicActionGasFeeSelection::Auto, None) => {
                state.status = WalletConnectFeeStatus::AwaitingSimulation;
                let key = Arc::<str>::from(request_key);
                self.refresh_walletconnect_gas_fee_quote(key, cx);
                return;
            }
            (
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas, ..
                },
                None,
            ) => Some(SelfBroadcastGasFeeQuote::from_rpc_gas_price(
                max_fee_per_gas,
            )),
            (_, quote) => quote,
        };
        let Some(quote) = quote else {
            state.status = WalletConnectFeeStatus::AwaitingSimulation;
            return;
        };
        let chain_id = match parse_chain_id(&request.item.chain_id) {
            Ok(chain_id) => chain_id,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                state.simulation_requested = false;
                return;
            }
        };
        let Ok(resolved) = resolve_public_action_gas_fee(
            chain_id,
            PublicShieldTransactionProfile::Railoxide,
            selection,
            Some(quote),
        ) else {
            state.status = WalletConnectFeeStatus::UnavailableFailed;
            state.simulation_requested = false;
            return;
        };
        state.status = WalletConnectFeeStatus::AwaitingSimulation;
        state.begin_attempt(self.walletconnect.walletconnect_gas_fee.refresh_id);
        let guard = state.completion_guard();
        let payload_fingerprint = state.payload_fingerprint;
        let review_token = state.review_token;
        let request_generation = state.request_generation;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let mut estimate_request =
            match walletconnect_transaction_estimate_request(&request, chain_id, effective_chain) {
                Ok(request) => request,
                Err(error) => {
                    state.status = WalletConnectFeeStatus::UnavailableFailed;
                    state.error = Some(Arc::from(error));
                    state.simulation_requested = false;
                    return;
                }
            };
        estimate_request.gas_fee = selection;
        let http = self.http.clone();
        let network_mode = http.network_mode();
        let retry_attempt = state.retry_attempt;
        let request_key = Arc::<str>::from(request_key);
        cx.spawn(async move |this, cx| {
            let result = walletconnect_await_before_request_expiry(
                request.item.expiry_timestamp,
                simulate_walletconnect_transaction(estimate_request, quote, resolved, &http),
            )
            .await
            .unwrap_or_else(|_| {
                WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(
                    Arc::from("WalletConnect request expired before simulation completed"),
                ))
            });
            let _ = this.update(cx, |root, _cx| {
                let current = !root
                    .walletconnect
                    .request_actions
                    .contains(request_key.as_ref())
                    && root
                        .walletconnect
                        .pending_requests
                        .get(request_key.as_ref())
                        .is_some_and(|request| {
                            request.review_token == review_token
                                && request.item.expiry_timestamp
                                    == root
                                        .walletconnect
                                        .walletconnect_fee_state
                                        .as_ref()
                                        .and_then(|state| state.expiry_timestamp)
                                && walletconnect_request_payload_fingerprint(request)
                                    .is_ok_and(|fingerprint| fingerprint == payload_fingerprint)
                        })
                    && root
                        .walletconnect
                        .walletconnect_fee_state
                        .as_ref()
                        .is_some_and(|state| {
                            state.is_completion_current(
                                request_key.as_ref(),
                                review_token,
                                payload_fingerprint,
                                guard,
                                root.walletconnect.request_dialog_open
                                    && root.walletconnect.request_dialog_key.as_deref()
                                        == Some(request_key.as_ref())
                                    && root.walletconnect.walletconnect_fee_request_generation
                                        == request_generation
                                    && root.walletconnect.walletconnect_gas_fee.refresh_id
                                        == guard.editor_generation,
                                current_unix_seconds(),
                            )
                        });
                if !current {
                    tracing::debug!(
                        chain_id,
                        network_mode = %network_mode,
                        attempt = retry_attempt,
                        "WalletConnect simulation completion discarded as stale"
                    );
                    return;
                }
                let native_usd_micro_rate = root
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_rate(chain_id);
                let Some(state) = root.walletconnect.walletconnect_fee_state.as_mut() else {
                    return;
                };
                state.simulation_requested = false;
                match result {
                    WalletConnectSimulationResult::Complete(estimate) => {
                        let projection = estimate.fee_projection(native_usd_micro_rate);
                        state.apply_successful_simulation_projection(projection);
                        state.error = None;
                        state.simulation_retryable = false;
                    }
                    WalletConnectSimulationResult::Error(
                        WalletConnectSimulationError::Reverted(error),
                    ) => {
                        state.status = WalletConnectFeeStatus::WouldRevert;
                        state.error = Some(error);
                        state.simulation_retryable = false;
                    }
                    WalletConnectSimulationResult::Error(
                        WalletConnectSimulationError::Unavailable(error),
                    ) => {
                        state.status = WalletConnectFeeStatus::UnavailableFailed;
                        state.error = Some(error);
                    }
                }
            });
        })
        .detach();
    }
}

fn parse_chain_id(value: &str) -> Result<u64, String> {
    if !value.starts_with("eip155:") {
        return Err("WalletConnect request is not EIP-155".to_owned());
    }
    parse_caip2_chain_id(value).ok_or_else(|| "WalletConnect request chain is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use wallet_ops::WalletConnectEvmTransaction;

    fn test_fee_state() -> WalletConnectFeeState {
        WalletConnectFeeState {
            request_key: Arc::from("topic:7"),
            review_token: 4,
            payload_fingerprint: B256::ZERO,
            request_generation: 1,
            dialog_generation: 2,
            editor_generation: 3,
            expiry_timestamp: None,
            status: WalletConnectFeeStatus::Fetching,
            error: None,
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        }
    }

    fn test_projection(
        source: PublicActionFeeSource,
        expected_gas_cost: u64,
    ) -> PublicActionFeeProjection {
        PublicActionFeeProjection {
            source,
            raw_gas_limit: 21_000,
            gas_limit: 31_000,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 2,
            expected_fee_per_gas: 15,
            expected_gas_cost: U256::from(expected_gas_cost),
            maximum_gas_cost: U256::from(expected_gas_cost + 10),
            expected_native_usd_micro_value: None,
            maximum_native_usd_micro_value: None,
        }
    }

    #[test]
    fn transaction_fee_review_pairing_matches_request_kind() {
        let transaction = WalletConnectParsedRequest::EthSendTransaction {
            transaction: WalletConnectEvmTransaction {
                from: Address::ZERO,
                to: None,
                value: None,
                data: None,
                access_list: None,
                gas: None,
                gas_price: None,
                max_fee_per_gas: None,
                max_priority_fee_per_gas: None,
                chain_id: None,
                nonce: None,
                transaction_type: None,
                raw: serde_json::Value::Null,
            },
        };
        let personal = WalletConnectParsedRequest::PersonalSign {
            message: "hello".to_owned(),
            account: Address::ZERO,
        };
        let typed_data = WalletConnectParsedRequest::EthSignTypedDataV4 {
            account: Address::ZERO,
            typed_data: serde_json::Value::Null,
            domain_chain_id: None,
        };

        assert!(walletconnect_request_fee_eligible(&transaction));
        assert!(!walletconnect_request_fee_eligible(&personal));
        assert!(!walletconnect_request_fee_eligible(&typed_data));

        let reviewed_fee = WalletConnectReviewedFeeProjection::unresolved("topic:7", 1);
        assert!(
            validate_walletconnect_reviewed_fee_pairing(&transaction, Some(&reviewed_fee),).is_ok()
        );
        assert!(validate_walletconnect_reviewed_fee_pairing(&typed_data, None).is_ok());
        assert!(validate_walletconnect_reviewed_fee_pairing(&transaction, None).is_err());
        assert!(
            validate_walletconnect_reviewed_fee_pairing(&typed_data, Some(&reviewed_fee),).is_err()
        );
    }

    #[test]
    fn custom_projection_does_not_require_an_automatic_quote() {
        let (projection, resolved) = walletconnect_fee_projection(
            1,
            21_000,
            10_000,
            None,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 20,
                max_priority_fee_per_gas: 2,
            },
            None,
        )
        .expect("custom projection");
        assert_eq!(resolved.max_fee_per_gas, 20);
        assert_eq!(projection.raw_gas_limit, 21_000);
        assert_eq!(projection.gas_limit, 31_000);
    }

    #[test]
    fn active_refresh_uses_display_projection_without_authoritative_status() {
        let projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
        let mut state = test_fee_state();
        state.last_successful_display_projection = Some(projection);

        assert!(matches!(state.status, WalletConnectFeeStatus::Fetching));
        assert_eq!(state.authoritative_projection(), None);
        assert_eq!(
            walletconnect_fee_state_projection(&state, true),
            Some(projection)
        );
    }

    #[test]
    fn inactive_terminal_failure_does_not_use_display_projection() {
        let projection = test_projection(PublicActionFeeSource::NetworkSimulation, 315_000);
        let mut state = test_fee_state();
        state.status = WalletConnectFeeStatus::UnavailableFailed;
        state.last_successful_display_projection = Some(projection);

        assert_eq!(walletconnect_fee_state_projection(&state, false), None);
    }

    #[test]
    fn editor_reset_and_new_request_clear_display_projection() {
        let projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
        let mut state = test_fee_state();
        state.last_successful_display_projection = Some(projection);
        state.invalidate();
        assert_eq!(state.last_successful_display_projection, None);

        let replacement = test_fee_state();
        assert_eq!(replacement.last_successful_display_projection, None);
    }

    #[test]
    fn successful_projection_replaces_display_projection() {
        let old_projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
        let new_projection = test_projection(PublicActionFeeSource::NetworkSimulation, 420_000);
        let mut state = test_fee_state();
        state.apply_successful_operation_projection(old_projection);
        assert_eq!(
            state.status,
            WalletConnectFeeStatus::EstimatedFromOperation(old_projection)
        );
        assert_eq!(state.authoritative_projection(), Some(old_projection));
        assert_eq!(
            state.last_successful_display_projection,
            Some(old_projection)
        );

        state.apply_successful_simulation_projection(new_projection);

        assert_eq!(
            walletconnect_fee_state_projection(&state, false),
            Some(new_projection)
        );
        assert_eq!(state.authoritative_projection(), Some(new_projection));
        assert_eq!(
            state.status,
            WalletConnectFeeStatus::Simulated(new_projection)
        );
        assert_eq!(
            state.last_successful_display_projection,
            Some(new_projection)
        );
    }

    #[test]
    fn reviewed_fee_context_rejects_stale_request_and_editor_selection() {
        let mut reviewed = WalletConnectReviewedFeeProjection::unresolved("topic:7", 4);
        reviewed.payload_fingerprint = B256::from([7; 32]);
        reviewed.editor_generation = 3;
        reviewed.selection = PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 2,
        };
        assert!(walletconnect_reviewed_fee_request_context_matches(
            &reviewed,
            "topic:7",
            4,
            B256::from([7; 32]),
        ));
        assert!(!walletconnect_reviewed_fee_request_context_matches(
            &reviewed,
            "other:7",
            4,
            B256::from([7; 32]),
        ));
        assert!(walletconnect_reviewed_fee_editor_is_current(
            &reviewed,
            3,
            reviewed.selection,
        ));
        assert!(!walletconnect_reviewed_fee_editor_is_current(
            &reviewed,
            4,
            reviewed.selection,
        ));
        assert!(!walletconnect_reviewed_fee_editor_is_current(
            &reviewed,
            3,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 21,
                max_priority_fee_per_gas: 2,
            },
        ));
    }

    #[test]
    fn completion_guard_rejects_changed_lifecycle_fields() {
        let fingerprint = B256::ZERO;
        let state = WalletConnectFeeState {
            request_key: Arc::from("topic:7"),
            review_token: 4,
            payload_fingerprint: fingerprint,
            request_generation: 1,
            dialog_generation: 2,
            editor_generation: 3,
            expiry_timestamp: None,
            status: WalletConnectFeeStatus::Fetching,
            error: None,
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        };
        assert!(state.is_completion_current(
            "topic:7",
            4,
            fingerprint,
            state.completion_guard(),
            true,
            1_700_000_000,
        ));
        assert!(!state.is_completion_current(
            "topic:7",
            5,
            fingerprint,
            state.completion_guard(),
            true,
            1_700_000_000,
        ));
        assert!(!state.is_completion_current(
            "other:7",
            4,
            fingerprint,
            state.completion_guard(),
            true,
            1_700_000_000,
        ));
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            B256::from([1; 32]),
            state.completion_guard(),
            true,
            1_700_000_000,
        ));
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            fingerprint,
            state.completion_guard(),
            false,
            1_700_000_000,
        ));
        let mut expired = state;
        expired.expiry_timestamp = Some(1);
        assert!(!expired.is_completion_current(
            "topic:7",
            4,
            fingerprint,
            expired.completion_guard(),
            true,
            2,
        ));
    }

    #[test]
    fn genuine_editor_change_invalidates_old_attempt_and_terminal_retry_is_enabled() {
        let mut state = WalletConnectFeeState {
            request_key: Arc::from("topic:7"),
            review_token: 4,
            payload_fingerprint: B256::ZERO,
            request_generation: 1,
            dialog_generation: 2,
            editor_generation: 3,
            expiry_timestamp: None,
            status: WalletConnectFeeStatus::Fetching,
            error: None,
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        };
        let old_guard = state.completion_guard();
        state.invalidate();
        state.editor_generation = 4;
        state.status = WalletConnectFeeStatus::UnavailableFailed;
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            B256::ZERO,
            old_guard,
            true,
            1_700_000_000,
        ));
        assert!(walletconnect_fee_retry_action_enabled(
            state.status,
            false,
            false,
        ));
        assert!(!walletconnect_fee_retry_action_enabled(
            state.status,
            true,
            false,
        ));
        state.begin_manual_retry();
        assert!(walletconnect_fee_retrying(state.retry_attempt, true, false,));
        assert!(!walletconnect_fee_retrying(
            state.retry_attempt,
            false,
            false,
        ));
    }

    #[test]
    fn late_completion_is_discarded_after_navigation_reopen_or_editor_change() {
        let mut state = WalletConnectFeeState {
            request_key: Arc::from("topic:7"),
            review_token: 4,
            payload_fingerprint: B256::ZERO,
            request_generation: 1,
            dialog_generation: 2,
            editor_generation: 3,
            expiry_timestamp: None,
            status: WalletConnectFeeStatus::Fetching,
            error: None,
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        };
        let old_guard = state.completion_guard();
        let mut navigation_guard = old_guard;
        navigation_guard.request_generation += 1;
        let mut reopen_guard = old_guard;
        reopen_guard.dialog_generation += 1;
        let mut editor_guard = old_guard;
        editor_guard.editor_generation += 1;
        let mut late_attempt_guard = old_guard;
        late_attempt_guard.generation += 1;
        for guard in [
            navigation_guard,
            reopen_guard,
            editor_guard,
            late_attempt_guard,
        ] {
            assert!(!state.is_completion_current(
                "topic:7",
                4,
                B256::ZERO,
                guard,
                true,
                1_700_000_000,
            ));
        }

        let current_guard = state.begin_attempt(4);
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            B256::ZERO,
            old_guard,
            true,
            1_700_000_000,
        ));
        assert!(state.is_completion_current(
            "topic:7",
            4,
            B256::ZERO,
            current_guard,
            true,
            1_700_000_000,
        ));

        state.invalidate();
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            B256::ZERO,
            current_guard,
            true,
            1_700_000_000,
        ));
        assert!(!state.is_completion_current(
            "topic:7",
            4,
            B256::ZERO,
            current_guard,
            false,
            1_700_000_000,
        ));
    }
}
