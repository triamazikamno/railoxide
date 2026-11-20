use std::sync::Arc;

use alloy::primitives::{B256, TxKind, U256};
use gpui::{Context, Focusable, Window};
use wallet_ops::{
    HttpContext, OperationHttpClientSlot, PublicActionFeeProjection, PublicActionFeeSource,
    PublicActionGasFeeQuote, PublicActionGasFeeSelection, PublicActionResolvedGasFee,
    PublicAdvancedTransactionEstimate, PublicAdvancedTransactionEstimateRequest,
    PublicAdvancedTransactionSimulationError, PublicAssetId, PublicShieldTransactionProfile,
    PublicTransactionIntent, SelfBroadcastGasFeeQuote, WalletConnectDecodedCallKind,
    WalletConnectParsedRequest, WalletConnectReviewedFee, WalletConnectReviewedTransaction,
    project_public_action_fee, public_native_action_gas_units_from_walletconnect_intent,
    quote_public_action_gas_fee_with_reads, resolve_public_action_gas_fee,
    simulate_public_advanced_transaction_with_fee_and_reads,
};

use super::helpers::{
    current_unix_seconds, parse_caip2_chain_id, walletconnect_await_before_request_expiry,
    walletconnect_duration_until_expiry,
};
use super::requests::transaction_request_from_walletconnect;
use super::{WalletConnectRequestUi, WalletRoot};
use crate::root::retry::retry_backoff_delay_capped;

mod editing;
mod refresh;
mod review;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

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
            expiry_timestamp: request.timeout_timestamp(),
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
    native_decimals: u8,
) -> Result<(PublicActionFeeProjection, PublicActionResolvedGasFee), String> {
    let resolved = resolve_public_action_gas_fee(
        chain_id,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
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
            native_decimals,
        ),
        resolved,
    ))
}

pub(super) fn walletconnect_transaction_estimate_request(
    request: &WalletConnectRequestUi,
    chain_id: u64,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
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
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: HttpContext,
    expiry_timestamp: Option<u64>,
    rpc_reads: Option<wallet_ops::DappRpcReadClient>,
    request_control: Option<wallet_ops::dapp_request::DappRequestControl>,
) -> Result<PublicActionGasFeeQuote, String> {
    const MAX_ATTEMPTS_WITHOUT_EXPIRY: u8 = 4;
    let mut attempt = 0;
    let network_mode = http.network_mode();
    loop {
        if request_control
            .as_ref()
            .is_some_and(|control| control.ensure_current().is_err())
        {
            return Err("Dapp approval is no longer current".to_owned());
        }
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
        let quote = async {
            match remaining {
                Some(remaining) => {
                    if let Ok(result) = tokio::time::timeout(
                        remaining,
                        quote_public_action_gas_fee_with_reads(
                            chain_id,
                            &effective_chain,
                            &http,
                            rpc_reads.as_ref(),
                        ),
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
                        Err(eyre::eyre!(
                            "WalletConnect request expired while fetching fee quote"
                        ))
                    }
                }
                None => {
                    quote_public_action_gas_fee_with_reads(
                        chain_id,
                        &effective_chain,
                        &http,
                        rpc_reads.as_ref(),
                    )
                    .await
                }
            }
        };
        let result = if let Some(control) = &request_control {
            tokio::select! {
                result = quote => result,
                () = control.cancelled() => return Err("Dapp approval is no longer current".to_owned()),
            }
        } else {
            quote.await
        };
        if request_control
            .as_ref()
            .is_some_and(|control| control.ensure_current().is_err())
        {
            return Err("Dapp approval is no longer current".to_owned());
        }
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
                if matches!(
                    error.downcast_ref::<wallet_ops::RpcBrokerError>(),
                    Some(
                        wallet_ops::RpcBrokerError::OriginRejected
                            | wallet_ops::RpcBrokerError::Shutdown
                    )
                ) =>
            {
                return Err(error.to_string());
            }
            Err(error)
                if expiry_timestamp.is_none()
                    && request_control.is_none()
                    && attempt + 1 >= MAX_ATTEMPTS_WITHOUT_EXPIRY =>
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
                if let Some(control) = &request_control {
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {},
                        () = control.cancelled() => return Err("Dapp approval is no longer current".to_owned()),
                    }
                } else {
                    tokio::time::sleep(wait).await;
                }
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
    operation: &OperationHttpClientSlot,
    rpc_reads: Option<&wallet_ops::DappRpcReadClient>,
) -> WalletConnectSimulationResult {
    let client = match operation.client().await {
        Ok(client) => client,
        Err(error) => {
            return WalletConnectSimulationResult::Error(
                WalletConnectSimulationError::Unavailable(Arc::from(error.to_string())),
            );
        }
    };
    match simulate_public_advanced_transaction_with_fee_and_reads(
        request, quote, resolved, http, &client, rpc_reads,
    )
    .await
    {
        Ok(estimate) => WalletConnectSimulationResult::Complete(estimate),
        Err(PublicAdvancedTransactionSimulationError::RpcRead(_)) => {
            WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(
                Arc::from("RPC read is unavailable."),
            ))
        }
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

fn parse_chain_id(value: &str) -> Result<u64, String> {
    if !value.starts_with("eip155:") {
        return Err("WalletConnect request is not EIP-155".to_owned());
    }
    parse_caip2_chain_id(value).ok_or_else(|| "WalletConnect request chain is invalid".to_owned())
}

#[cfg(test)]
mod tests;
