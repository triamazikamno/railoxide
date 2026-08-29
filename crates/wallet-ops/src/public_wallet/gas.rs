use std::collections::HashMap;

use alloy::network::TransactionBuilder as _;
use alloy::primitives::{B256, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::BlockNumberOrTag;
use alloy::sol_types::{Panic, Revert, SolError, decode_revert_reason};
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, WrapErr, eyre};

use super::actions::{public_send_transaction_request, validate_public_transaction_intent};
use super::runtime::public_chain_runtime_config;
use super::types::{
    PublicActionFeeProjection, PublicActionFeeSource, PublicActionGasFeeQuote,
    PublicActionGasFeeQuoteBundle, PublicActionGasFeeSelection, PublicActionKind,
    PublicActionProgressStep, PublicActionResolvedGasFee, PublicAdvancedTransactionEstimate,
    PublicAdvancedTransactionEstimateRequest, PublicAdvancedTransactionSimulationError,
    PublicAssetId, PublicShieldTransactionProfile, PublicTransactionIntent,
};
use crate::settings::EffectiveChainConfig;
use crate::walletconnect::WalletConnectDecodedCallKind;
use crate::{
    Eip1559GasCostProjection, GAS_LIMIT_BUFFER, HttpContext, RAILGUN_PROTOCOL_FEE_BPS,
    SelfBroadcastGasFeeQuote, SelfBroadcastResolvedGasFee, SelfBroadcastTipFallback,
    expected_eip1559_fee_per_gas, query_rpc_pool_with_http_client, railgun_protocol_fee_amount,
    resolve_self_broadcast_gas_fee, self_broadcast_gas_fee_quote_from_rpc_pool_with_tip_fallback,
};
use railgun_ui::native_usd_micro_value;

pub(super) const PUBLIC_NATIVE_SEND_GAS_UNITS: u64 = 21_000;
pub(super) const PUBLIC_ERC20_SEND_GAS_UNITS: u64 = 65_000;
pub(super) const PUBLIC_NATIVE_WRAP_GAS_UNITS: u64 = 50_000;
pub const PUBLIC_NATIVE_UNWRAP_GAS_UNITS: u64 = 40_000;
pub(super) const PUBLIC_NATIVE_APPROVE_GAS_UNITS: u64 = 65_000;
pub(super) const PUBLIC_NATIVE_SHIELD_GAS_UNITS: u64 = 650_000;
pub(super) const PUBLIC_NATIVE_RELAY_ADAPT_SHIELD_GAS_UNITS: u64 = 900_000;
pub(super) const PUBLIC_RAILWAY_NATIVE_SHIELD_GAS_UNITS: u64 = 6_000_000;
const PUBLIC_ACTION_BNB_CHAIN_ID: u64 = 56;
const RAILWAY_FEE_HISTORY_BLOCKS: u64 = 10;
const RAILWAY_FEE_HISTORY_REWARD_PERCENTILES: [f64; 4] = [40.0, 60.0, 80.0, 95.0];
const RAILWAY_BNB_GAS_PRICE_CAP: u128 = 50_000_000;

#[must_use]
pub fn public_native_action_gas_units(steps: &[PublicActionProgressStep]) -> u64 {
    public_native_action_gas_units_with_buffer(steps, GAS_LIMIT_BUFFER)
}

#[must_use]
pub(super) fn public_native_action_gas_units_with_buffer(
    steps: &[PublicActionProgressStep],
    gas_limit_buffer: u64,
) -> u64 {
    steps.iter().fold(0_u64, |total, step| {
        let gas_units = public_native_step_gas_units(*step);
        if gas_units == 0 {
            total
        } else {
            total.saturating_add(gas_units + gas_limit_buffer)
        }
    })
}

#[must_use]
pub fn public_native_action_gas_reserve(
    max_fee_per_gas: u128,
    steps: &[PublicActionProgressStep],
) -> U256 {
    public_native_action_gas_reserve_with_buffer(max_fee_per_gas, steps, GAS_LIMIT_BUFFER)
}

pub fn estimate_public_action_gas_cost(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    kind: PublicActionKind,
    asset: PublicAssetId,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<Eip1559GasCostProjection> {
    estimate_public_action_gas_cost_with_profile(
        chain_id,
        effective_chain,
        kind,
        asset,
        PublicShieldTransactionProfile::Railoxide,
        gas_fee,
        quote,
    )
}

pub fn estimate_public_action_gas_cost_with_profile(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<Eip1559GasCostProjection> {
    estimate_public_action_gas_cost_with_profile_and_ceiling(
        chain_id,
        effective_chain,
        kind,
        asset,
        profile,
        gas_fee,
        quote,
        None,
    )
}

pub fn estimate_public_action_gas_cost_with_profile_and_ceiling(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
    authorization_ceiling: Option<PublicActionGasFeeSelection>,
) -> Result<Eip1559GasCostProjection> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let resolved = resolve_public_action_gas_fee(chain_id, profile, gas_fee, quote)?;
    let maximum_resolved = authorization_ceiling.map_or(Ok(resolved), |ceiling| {
        resolve_public_action_gas_fee(chain_id, profile, ceiling, None)
    })?;
    let expected_gas_units = public_action_estimated_gas_usage_units(kind, asset);
    let maximum_gas_units = public_action_estimated_gas_units_with_buffer(
        kind,
        asset,
        profile,
        chain.gas.gas_limit_buffer,
    );
    if profile.uses_legacy_envelope(chain_id) {
        return Ok(legacy_gas_cost_projection(
            expected_gas_units,
            maximum_gas_units,
            resolved.max_fee_per_gas,
            maximum_resolved.max_fee_per_gas,
        ));
    }
    let quote = quote
        .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(resolved.max_fee_per_gas));
    Ok(public_action_eip1559_gas_cost_projection(
        expected_gas_units,
        maximum_gas_units,
        quote,
        resolved.max_fee_per_gas,
        resolved.max_priority_fee_per_gas,
        maximum_resolved.max_fee_per_gas,
    ))
}

#[must_use]
fn public_action_eip1559_gas_cost_projection(
    expected_gas_units: u64,
    maximum_gas_units: u64,
    quote: PublicActionGasFeeQuote,
    expected_max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    maximum_fee_per_gas: u128,
) -> Eip1559GasCostProjection {
    let expected_fee_per_gas =
        expected_eip1559_fee_per_gas(quote, expected_max_fee_per_gas, max_priority_fee_per_gas);
    Eip1559GasCostProjection {
        expected_fee_per_gas,
        maximum_fee_per_gas,
        expected_cost: U256::from(expected_gas_units) * U256::from(expected_fee_per_gas),
        maximum_cost: U256::from(maximum_gas_units) * U256::from(maximum_fee_per_gas),
    }
}

#[must_use]
pub const fn public_native_action_gas_units_from_walletconnect_intent(
    kind: &WalletConnectDecodedCallKind,
) -> Option<u64> {
    match kind {
        WalletConnectDecodedCallKind::NativeTransfer => Some(PUBLIC_NATIVE_SEND_GAS_UNITS),
        WalletConnectDecodedCallKind::Erc20Approve { .. }
        | WalletConnectDecodedCallKind::Erc20Transfer { .. }
        | WalletConnectDecodedCallKind::Erc20TransferFrom { .. } => {
            Some(PUBLIC_ERC20_SEND_GAS_UNITS)
        }
        WalletConnectDecodedCallKind::WrappedDeposit => Some(PUBLIC_NATIVE_WRAP_GAS_UNITS),
        WalletConnectDecodedCallKind::WrappedWithdraw { .. } => {
            Some(PUBLIC_NATIVE_UNWRAP_GAS_UNITS)
        }
        WalletConnectDecodedCallKind::ContractCall { .. }
        | WalletConnectDecodedCallKind::ContractCreation => None,
    }
}

#[must_use]
pub fn public_walletconnect_operation_gas_limit(
    kind: &WalletConnectDecodedCallKind,
    gas_limit_buffer: u64,
) -> Option<u64> {
    public_native_action_gas_units_from_walletconnect_intent(kind)
        .map(|raw_gas| raw_gas.saturating_add(gas_limit_buffer))
}

#[must_use]
pub fn public_action_maximum_gas_cost_is_significant(
    expected_gas_cost: U256,
    maximum_gas_cost: U256,
) -> bool {
    if maximum_gas_cost <= expected_gas_cost {
        return false;
    }
    (maximum_gas_cost - expected_gas_cost)
        .checked_mul(U256::from(10_u8))
        .is_none_or(|scaled_difference| scaled_difference >= expected_gas_cost)
}

#[must_use]
pub fn project_public_action_fee(
    raw_gas_limit: u64,
    gas_limit: u64,
    quote: PublicActionGasFeeQuote,
    resolved_fee: PublicActionResolvedGasFee,
    source: PublicActionFeeSource,
    native_usd_micro_rate: Option<U256>,
) -> PublicActionFeeProjection {
    let projection = public_action_eip1559_gas_cost_projection(
        raw_gas_limit,
        gas_limit,
        quote,
        resolved_fee.max_fee_per_gas,
        resolved_fee.max_priority_fee_per_gas,
        resolved_fee.max_fee_per_gas,
    );
    PublicActionFeeProjection {
        source,
        raw_gas_limit,
        gas_limit,
        max_fee_per_gas: resolved_fee.max_fee_per_gas,
        max_priority_fee_per_gas: resolved_fee.max_priority_fee_per_gas,
        expected_fee_per_gas: projection.expected_fee_per_gas,
        expected_gas_cost: projection.expected_cost,
        maximum_gas_cost: projection.maximum_cost,
        expected_native_usd_micro_value: native_usd_micro_rate
            .and_then(|rate| native_usd_micro_value(projection.expected_cost, rate)),
        maximum_native_usd_micro_value: native_usd_micro_rate
            .and_then(|rate| native_usd_micro_value(projection.maximum_cost, rate)),
    }
}

impl PublicAdvancedTransactionEstimate {
    #[must_use]
    pub fn fee_projection(&self, native_usd_micro_rate: Option<U256>) -> PublicActionFeeProjection {
        PublicActionFeeProjection {
            source: PublicActionFeeSource::NetworkSimulation,
            raw_gas_limit: self.raw_gas_limit,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            expected_fee_per_gas: self.expected_fee_per_gas,
            expected_gas_cost: self.expected_gas_cost,
            maximum_gas_cost: self.max_gas_cost,
            expected_native_usd_micro_value: native_usd_micro_rate
                .and_then(|rate| native_usd_micro_value(self.expected_gas_cost, rate)),
            maximum_native_usd_micro_value: native_usd_micro_rate
                .and_then(|rate| native_usd_micro_value(self.max_gas_cost, rate)),
        }
    }
}

#[must_use]
fn legacy_gas_cost_projection(
    expected_gas_units: u64,
    maximum_gas_units: u64,
    expected_gas_price: u128,
    maximum_gas_price: u128,
) -> Eip1559GasCostProjection {
    Eip1559GasCostProjection {
        expected_fee_per_gas: expected_gas_price,
        maximum_fee_per_gas: maximum_gas_price,
        expected_cost: U256::from(expected_gas_units) * U256::from(expected_gas_price),
        maximum_cost: U256::from(maximum_gas_units) * U256::from(maximum_gas_price),
    }
}

pub fn resolve_public_action_gas_fee(
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<SelfBroadcastResolvedGasFee> {
    let quote = match quote {
        Some(quote) => quote,
        None => match gas_fee {
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas, ..
            } => SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas),
            PublicActionGasFeeSelection::Auto => {
                return Err(eyre!("public action gas fee quote is not ready"));
            }
        },
    };
    let resolved = resolve_self_broadcast_gas_fee(gas_fee, quote)?;
    if !profile.uses_legacy_envelope(chain_id) {
        return Ok(resolved);
    }

    let gas_price = match gas_fee {
        PublicActionGasFeeSelection::Auto => quote.rpc_gas_price,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas, ..
        } => max_fee_per_gas,
    };
    if gas_price == 0 {
        return Err(eyre!("legacy gas price must be greater than zero"));
    }
    Ok(SelfBroadcastResolvedGasFee {
        rpc_gas_price: quote.rpc_gas_price,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: 0,
    })
}

#[must_use]
pub fn public_shield_protocol_fee_amount(amount: U256) -> U256 {
    railgun_protocol_fee_amount(amount, RAILGUN_PROTOCOL_FEE_BPS)
}

fn public_action_estimated_gas_units_with_buffer(
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
) -> u64 {
    match kind {
        PublicActionKind::Send => {
            let gas_units = match asset {
                PublicAssetId::Native => PUBLIC_NATIVE_SEND_GAS_UNITS,
                PublicAssetId::Erc20(_) => PUBLIC_ERC20_SEND_GAS_UNITS,
            };
            gas_units.saturating_add(gas_limit_buffer)
        }
        PublicActionKind::Shield => match asset {
            PublicAssetId::Native => match profile {
                PublicShieldTransactionProfile::Railway => PUBLIC_RAILWAY_NATIVE_SHIELD_GAS_UNITS,
                PublicShieldTransactionProfile::Railoxide => {
                    PUBLIC_NATIVE_RELAY_ADAPT_SHIELD_GAS_UNITS.saturating_add(gas_limit_buffer)
                }
            },
            PublicAssetId::Erc20(_) => match profile {
                PublicShieldTransactionProfile::Railway => {
                    railway_gas_limit(PUBLIC_NATIVE_APPROVE_GAS_UNITS)
                        .saturating_add(railway_gas_limit(PUBLIC_NATIVE_SHIELD_GAS_UNITS))
                }
                PublicShieldTransactionProfile::Railoxide => PUBLIC_NATIVE_APPROVE_GAS_UNITS
                    .saturating_add(gas_limit_buffer)
                    .saturating_add(PUBLIC_NATIVE_SHIELD_GAS_UNITS)
                    .saturating_add(gas_limit_buffer),
            },
        },
    }
}

pub(super) const fn public_action_estimated_gas_usage_units(
    kind: PublicActionKind,
    asset: PublicAssetId,
) -> u64 {
    match kind {
        PublicActionKind::Send => match asset {
            PublicAssetId::Native => PUBLIC_NATIVE_SEND_GAS_UNITS,
            PublicAssetId::Erc20(_) => PUBLIC_ERC20_SEND_GAS_UNITS,
        },
        PublicActionKind::Shield => match asset {
            PublicAssetId::Native => PUBLIC_NATIVE_RELAY_ADAPT_SHIELD_GAS_UNITS,
            PublicAssetId::Erc20(_) => {
                PUBLIC_NATIVE_APPROVE_GAS_UNITS + PUBLIC_NATIVE_SHIELD_GAS_UNITS
            }
        },
    }
}

#[must_use]
fn public_native_action_gas_reserve_with_buffer(
    max_fee_per_gas: u128,
    steps: &[PublicActionProgressStep],
    gas_limit_buffer: u64,
) -> U256 {
    public_native_action_gas_reserve_with_profile(
        max_fee_per_gas,
        steps,
        PublicShieldTransactionProfile::Railoxide,
        gas_limit_buffer,
    )
}

#[must_use]
pub(super) fn public_native_action_gas_reserve_with_profile(
    max_fee_per_gas: u128,
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
) -> U256 {
    U256::from(public_native_action_gas_units_with_profile(
        steps,
        profile,
        gas_limit_buffer,
    )) * U256::from(max_fee_per_gas)
}

#[must_use]
fn public_native_action_gas_units_with_profile(
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
) -> u64 {
    steps.iter().fold(0_u64, |total, step| {
        let gas_units = if profile == PublicShieldTransactionProfile::Railway
            && *step == PublicActionProgressStep::Shield
        {
            PUBLIC_RAILWAY_NATIVE_SHIELD_GAS_UNITS
        } else {
            public_native_step_gas_units(*step)
        };
        if gas_units == 0 {
            total
        } else if profile == PublicShieldTransactionProfile::Railway
            && *step == PublicActionProgressStep::Shield
        {
            total.saturating_add(gas_units)
        } else {
            total.saturating_add(gas_units.saturating_add(gas_limit_buffer))
        }
    })
}

#[must_use]
pub(super) fn railway_gas_limit(estimated_gas: u64) -> u64 {
    let multiplied = u128::from(estimated_gas) * 120 / 100;
    multiplied.min(u128::from(u64::MAX)) as u64
}

pub async fn quote_public_action_gas_fee(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuote> {
    quote_public_action_gas_fee_with_profile(
        chain_id,
        effective_chain,
        PublicShieldTransactionProfile::Railoxide,
        http,
    )
    .await
}

pub async fn quote_public_action_gas_fee_with_profile(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    profile: PublicShieldTransactionProfile,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuote> {
    Ok(
        quote_public_action_gas_fee_bundle_with_profile(chain_id, effective_chain, profile, http)
            .await?
            .standard,
    )
}

pub async fn quote_public_action_gas_fee_bundle_with_profile(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    profile: PublicShieldTransactionProfile,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuoteBundle> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    public_action_gas_fee_quote_bundle_from_rpc_pool_with_profile(
        &query_rpc_pool,
        http.network_mode(),
        chain_id,
        profile,
    )
    .await
}

pub async fn estimate_public_advanced_transaction(
    request: PublicAdvancedTransactionEstimateRequest,
    http: &HttpContext,
) -> Result<PublicAdvancedTransactionEstimate> {
    validate_public_transaction_intent(&request.intent)?;
    if !matches!(request.intent, PublicTransactionIntent::Raw { .. }) {
        return Err(eyre!(
            "advanced gas estimation requires a raw transaction intent"
        ));
    }
    let chain = public_chain_runtime_config(request.chain_id, request.effective_chain.as_ref())?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let quote = public_action_gas_fee_quote_from_rpc_pool(
        &query_rpc_pool,
        http.network_mode(),
        request.chain_id,
    )
    .await
    .wrap_err("fetch advanced public transaction gas price")?;
    let resolved = resolve_public_action_gas_fee(
        request.chain_id,
        PublicShieldTransactionProfile::Railoxide,
        request.gas_fee,
        Some(quote),
    )?;
    estimate_public_advanced_transaction_with_fee(request, quote, resolved, http).await
}

pub async fn estimate_public_advanced_transaction_with_fee(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
) -> Result<PublicAdvancedTransactionEstimate> {
    validate_advanced_transaction_estimate_request(&request)?;
    estimate_public_advanced_transaction_with_fee_core(request, quote, resolved, http)
        .await
        .map_err(|error| eyre!(simulation_error_reason(&error).to_owned()))
        .wrap_err("all advanced public transaction query RPC attempts failed")
}

pub async fn simulate_public_advanced_transaction_with_fee(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    validate_advanced_transaction_estimate_request(&request).map_err(|error| {
        PublicAdvancedTransactionSimulationError::Unavailable(error.to_string())
    })?;
    estimate_public_advanced_transaction_with_fee_core(request, quote, resolved, http).await
}

fn validate_advanced_transaction_estimate_request(
    request: &PublicAdvancedTransactionEstimateRequest,
) -> Result<()> {
    validate_public_transaction_intent(&request.intent)?;
    if !matches!(request.intent, PublicTransactionIntent::Raw { .. }) {
        return Err(eyre!(
            "advanced gas estimation requires a raw transaction intent"
        ));
    }
    Ok(())
}

async fn estimate_public_advanced_transaction_with_fee_core(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    let chain = public_chain_runtime_config(request.chain_id, request.effective_chain.as_ref())
        .map_err(|_| unavailable_simulation_error("RPC providers are unavailable."))?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let mut tx_req =
        public_send_transaction_request(request.chain_id, request.from, &request.intent)
            .map_err(|error| unavailable_simulation_error(&error.to_string()))?
            .with_max_fee_per_gas(resolved.max_fee_per_gas)
            .with_max_priority_fee_per_gas(resolved.max_priority_fee_per_gas);
    tx_req.access_list = request.access_list;

    let providers = query_rpc_pool.available_providers();
    if providers.is_empty() {
        return Err(unavailable_simulation_error(
            "RPC providers are unavailable.",
        ));
    }

    let mut failures = Vec::with_capacity(providers.len());
    for provider_handle in providers {
        match provider_handle.provider.estimate_gas(tx_req.clone()).await {
            Ok(estimated_gas) => {
                let gas_limit =
                    buffered_advanced_gas_limit(estimated_gas, chain.gas.gas_limit_buffer);
                let projection = project_public_action_fee(
                    estimated_gas,
                    gas_limit,
                    quote,
                    resolved,
                    PublicActionFeeSource::NetworkSimulation,
                    None,
                );
                return Ok(PublicAdvancedTransactionEstimate {
                    payload_fingerprint: public_advanced_transaction_payload_fingerprint(
                        request.chain_id,
                        request.from,
                        &request.intent,
                        resolved.max_fee_per_gas,
                        resolved.max_priority_fee_per_gas,
                    ),
                    raw_gas_limit: estimated_gas,
                    gas_limit,
                    max_fee_per_gas: resolved.max_fee_per_gas,
                    max_priority_fee_per_gas: resolved.max_priority_fee_per_gas,
                    expected_fee_per_gas: projection.expected_fee_per_gas,
                    expected_gas_cost: projection.expected_gas_cost,
                    max_gas_cost: projection.maximum_gas_cost,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "advanced public transaction gas estimate failed");
                failures.push(classify_provider_failure(&error));
            }
        }
    }

    Err(select_provider_failure(failures))
}

#[derive(Debug, Eq, Hash, PartialEq)]
enum ProviderFailureGroup {
    RevertData(Vec<u8>),
    RevertMessage(String),
    JsonRpc { code: i64, message: String },
    Http(u16),
    Other,
}

#[derive(Debug)]
struct ProviderFailure {
    group: ProviderFailureGroup,
    reason: String,
}

fn classify_provider_failure(error: &alloy::transports::TransportError) -> ProviderFailure {
    if let Some(response) = error.as_error_resp() {
        if let Some(data) = response.as_revert_data() {
            let data = data.to_vec();
            return ProviderFailure {
                reason: friendly_revert_data(&data),
                group: ProviderFailureGroup::RevertData(data),
            };
        }
        if let Some(reason) = friendly_revert_message(&response.message) {
            return ProviderFailure {
                group: ProviderFailureGroup::RevertMessage(reason.to_ascii_lowercase()),
                reason,
            };
        }
        let message = safe_provider_message(&response.message);
        return ProviderFailure {
            group: ProviderFailureGroup::JsonRpc {
                code: response.code,
                message: message.to_ascii_lowercase(),
            },
            reason: if message.is_empty() {
                format!("RPC error {}.", response.code)
            } else {
                format!("RPC error {}: {message}", response.code)
            },
        };
    }

    if let Some(transport) = error.as_transport_err()
        && let Some(http) = transport.as_http_error()
    {
        return ProviderFailure {
            group: ProviderFailureGroup::Http(http.status),
            reason: format!("RPC returned HTTP status {}.", http.status),
        };
    }

    ProviderFailure {
        group: ProviderFailureGroup::Other,
        reason: "RPC provider is unavailable.".to_owned(),
    }
}

fn select_provider_failure(
    failures: Vec<ProviderFailure>,
) -> PublicAdvancedTransactionSimulationError {
    const CONFLICTING_RESULTS: &str = "RPC providers returned conflicting simulation results.";

    let mut groups = HashMap::<ProviderFailureGroup, (usize, String)>::new();
    for failure in failures {
        let entry = groups.entry(failure.group).or_insert((0, failure.reason));
        entry.0 += 1;
    }
    let Some((group, (highest_count, reason))) = groups.iter().max_by_key(|(_, (count, _))| *count)
    else {
        return unavailable_simulation_error("RPC providers are unavailable.");
    };
    if groups
        .values()
        .filter(|(count, _)| count == highest_count)
        .count()
        != 1
    {
        return unavailable_simulation_error(CONFLICTING_RESULTS);
    }
    match group {
        ProviderFailureGroup::RevertData(_) | ProviderFailureGroup::RevertMessage(_) => {
            PublicAdvancedTransactionSimulationError::Reverted(reason.clone())
        }
        ProviderFailureGroup::JsonRpc { .. }
        | ProviderFailureGroup::Http(_)
        | ProviderFailureGroup::Other => {
            PublicAdvancedTransactionSimulationError::Unavailable(reason.clone())
        }
    }
}

fn unavailable_simulation_error(reason: &str) -> PublicAdvancedTransactionSimulationError {
    PublicAdvancedTransactionSimulationError::Unavailable(reason.to_owned())
}

fn simulation_error_reason(error: &PublicAdvancedTransactionSimulationError) -> &str {
    match error {
        PublicAdvancedTransactionSimulationError::Reverted(reason)
        | PublicAdvancedTransactionSimulationError::Unavailable(reason) => reason,
    }
}

fn friendly_revert_data(data: &[u8]) -> String {
    let Some(selector) = data.get(..4) else {
        return "Execution reverted".to_owned();
    };
    if (selector == Revert::SELECTOR || selector == Panic::SELECTOR)
        && let Some(reason) = decode_revert_reason(data)
    {
        let reason = safe_provider_message(reason.strip_prefix("revert: ").unwrap_or(&reason));
        return if reason.is_empty() {
            "Execution reverted".to_owned()
        } else {
            reason
        };
    }
    format!("Custom error 0x{}", alloy::hex::encode(selector))
}

fn friendly_revert_message(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let marker = ["execution reverted", "reverted"]
        .into_iter()
        .filter_map(|marker| lower.find(marker).map(|index| (index, marker)))
        .min_by_key(|(index, _)| *index)?;
    let suffix = message[marker.0 + marker.1.len()..]
        .trim_start_matches([' ', ':', '-'])
        .trim();
    Some(if suffix.is_empty() {
        "Execution reverted".to_owned()
    } else {
        let reason = safe_provider_message(suffix);
        if reason.is_empty() {
            "Execution reverted".to_owned()
        } else {
            reason
        }
    })
}

fn safe_provider_message(message: &str) -> String {
    const MAX_MESSAGE_CHARS: usize = 160;
    let mut message = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if message.chars().count() > MAX_MESSAGE_CHARS {
        message = message.chars().take(MAX_MESSAGE_CHARS - 3).collect();
        message.push_str("...");
    }
    message
}

pub(super) const fn buffered_advanced_gas_limit(estimated_gas: u64, buffer: u64) -> u64 {
    estimated_gas.saturating_add(buffer)
}

pub(super) fn public_advanced_transaction_payload_fingerprint(
    chain_id: u64,
    from: alloy::primitives::Address,
    intent: &PublicTransactionIntent,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> B256 {
    let mut encoded = b"railoxide:public-advanced-transaction:v1".to_vec();
    encoded.extend_from_slice(&chain_id.to_be_bytes());
    encoded.extend_from_slice(from.as_slice());
    match intent {
        PublicTransactionIntent::Transfer {
            asset,
            amount,
            recipient,
        } => {
            encoded.push(0);
            match asset {
                PublicAssetId::Native => encoded.push(0),
                PublicAssetId::Erc20(token) => {
                    encoded.push(1);
                    encoded.extend_from_slice(token.as_slice());
                }
            }
            encoded.extend_from_slice(&amount.to_be_bytes::<32>());
            encoded.extend_from_slice(recipient.as_slice());
        }
        PublicTransactionIntent::Raw { to, value, data } => {
            encoded.push(1);
            match to {
                Some(to) => {
                    encoded.push(1);
                    encoded.extend_from_slice(to.as_slice());
                }
                None => encoded.push(0),
            }
            encoded.extend_from_slice(&value.to_be_bytes::<32>());
            encoded.extend_from_slice(&(data.len() as u64).to_be_bytes());
            encoded.extend_from_slice(data);
        }
    }
    encoded.extend_from_slice(&max_fee_per_gas.to_be_bytes());
    encoded.extend_from_slice(&max_priority_fee_per_gas.to_be_bytes());
    keccak256(encoded)
}

pub async fn estimate_public_native_action_gas_reserve(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    effective_chain: Option<&EffectiveChainConfig>,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
) -> Result<U256> {
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
        chain_id,
        steps,
        PublicShieldTransactionProfile::Railoxide,
        effective_chain,
        gas_fee,
        http,
        None,
    )
    .await
}

pub async fn estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    effective_chain: Option<&EffectiveChainConfig>,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
    authorization_ceiling: Option<PublicActionGasFeeSelection>,
) -> Result<U256> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let quote_bundle = public_action_gas_fee_quote_bundle_from_rpc_pool_with_profile(
        &query_rpc_pool,
        http.network_mode(),
        chain_id,
        profile,
    )
    .await
    .wrap_err("fetch public action gas price")?;
    let quote = quote_bundle.standard;
    let gas = resolve_public_action_gas_fee(chain_id, profile, gas_fee, Some(quote))?;
    let maximum_gas = authorization_ceiling.map_or(Ok(gas), |ceiling| {
        resolve_public_action_gas_fee(chain_id, profile, ceiling, None)
    })?;
    Ok(public_native_action_gas_reserve_with_profile(
        maximum_gas.max_fee_per_gas,
        steps,
        profile,
        chain.gas.gas_limit_buffer,
    ))
}

pub async fn estimate_public_native_action_gas_reserve_with_profile(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    effective_chain: Option<&EffectiveChainConfig>,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
) -> Result<U256> {
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
        chain_id,
        steps,
        profile,
        effective_chain,
        gas_fee,
        http,
        None,
    )
    .await
}

pub(super) async fn public_action_gas_fee_quote_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
) -> Result<PublicActionGasFeeQuote> {
    self_broadcast_gas_fee_quote_from_rpc_pool_with_tip_fallback(
        query_rpc_pool,
        network_mode,
        public_action_tip_fallback(chain_id),
    )
    .await
}

pub(super) async fn public_action_gas_fee_quote_from_rpc_pool_with_profile(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
) -> Result<PublicActionGasFeeQuote> {
    Ok(
        public_action_gas_fee_quote_bundle_from_rpc_pool_with_profile(
            query_rpc_pool,
            network_mode,
            chain_id,
            profile,
        )
        .await?
        .standard,
    )
}

pub(super) async fn public_action_gas_fee_quote_bundle_from_rpc_pool_with_profile(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
) -> Result<PublicActionGasFeeQuoteBundle> {
    if profile == PublicShieldTransactionProfile::Railoxide {
        let standard =
            public_action_gas_fee_quote_from_rpc_pool(query_rpc_pool, network_mode, chain_id)
                .await?;
        return Ok(public_action_gas_fee_quote_bundle_from_standard(standard));
    }
    let providers = query_rpc_pool.available_providers();
    if providers.is_empty() {
        return Err(eyre!("no healthy query RPC available"));
    }
    for provider_handle in providers {
        match railway_gas_fee_quote_from_provider(&provider_handle.provider, chain_id).await {
            Ok(quote) => return Ok(quote),
            Err(_) => {
                tracing::debug!("Railway gas fee quote provider attempt failed");
            }
        }
    }
    Err(eyre!("all Railway gas quote RPC attempts failed"))
}

async fn railway_gas_fee_quote_from_provider(
    provider: &impl Provider,
    chain_id: u64,
) -> Result<PublicActionGasFeeQuoteBundle> {
    if chain_id == PUBLIC_ACTION_BNB_CHAIN_ID {
        let provider_gas_price = provider
            .get_gas_price()
            .await
            .wrap_err("fetch Railway BNB gas price")?;
        return Ok(railway_bnb_gas_fee_quote_bundle(provider_gas_price));
    }
    let fee_history = provider
        .get_fee_history(
            RAILWAY_FEE_HISTORY_BLOCKS,
            BlockNumberOrTag::Latest,
            &RAILWAY_FEE_HISTORY_REWARD_PERCENTILES,
        )
        .await
        .wrap_err("fetch Railway fee history")?;
    railway_standard_gas_fee_quote_bundle(
        &fee_history.base_fee_per_gas,
        fee_history.reward.as_deref(),
    )
}

#[cfg(test)]
pub(super) fn railway_standard_gas_fee_quote(
    base_fee_per_gas: &[u128],
    rewards: Option<&[Vec<u128>]>,
) -> Result<PublicActionGasFeeQuote> {
    Ok(railway_standard_gas_fee_quote_bundle(base_fee_per_gas, rewards)?.standard)
}

pub(super) fn railway_standard_gas_fee_quote_bundle(
    base_fee_per_gas: &[u128],
    rewards: Option<&[Vec<u128>]>,
) -> Result<PublicActionGasFeeQuoteBundle> {
    let next_base_fee_per_gas = base_fee_per_gas
        .last()
        .copied()
        .ok_or_else(|| eyre!("Railway fee history returned no base fee"))?;
    let rewards = rewards.ok_or_else(|| eyre!("Railway fee history returned no rewards"))?;
    let mut priority_fees = rewards
        .iter()
        .map(|reward| reward.get(1).copied())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| eyre!("Railway fee history reward columns are incomplete"))?;
    let max_priority_fee_per_gas = railway_lower_median(&mut priority_fees)
        .ok_or_else(|| eyre!("Railway fee history returned no priority fees"))?;
    let mut aggressive_priority_fees = rewards
        .iter()
        .map(|reward| reward.get(3).copied())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| eyre!("Railway fee history reward columns are incomplete"))?;
    let aggressive_priority_fee_per_gas = railway_lower_median(&mut aggressive_priority_fees)
        .ok_or_else(|| eyre!("Railway fee history returned no priority fees"))?;
    let max_base_fee_per_gas = next_base_fee_per_gas
        .checked_mul(110)
        .ok_or_else(|| eyre!("Railway base fee overflow"))?
        / 100;
    let max_fee_per_gas = max_base_fee_per_gas
        .checked_add(max_priority_fee_per_gas)
        .ok_or_else(|| eyre!("Railway max fee overflow"))?;
    let aggressive_max_base_fee_per_gas = next_base_fee_per_gas
        .checked_mul(140)
        .ok_or_else(|| eyre!("Railway aggressive base fee overflow"))?
        / 100;
    let aggressive_max_fee_per_gas = aggressive_max_base_fee_per_gas
        .checked_add(aggressive_priority_fee_per_gas)
        .ok_or_else(|| eyre!("Railway aggressive max fee overflow"))?;
    let standard = PublicActionGasFeeQuote {
        rpc_gas_price: max_fee_per_gas,
        current_base_fee_per_gas: base_fee_per_gas
            .len()
            .checked_sub(2)
            .and_then(|index| base_fee_per_gas.get(index))
            .copied(),
        suggested_max_fee_per_gas: max_fee_per_gas,
        suggested_max_priority_fee_per_gas: max_priority_fee_per_gas,
    };
    Ok(PublicActionGasFeeQuoteBundle {
        standard,
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: aggressive_max_fee_per_gas,
            max_priority_fee_per_gas: aggressive_priority_fee_per_gas,
        },
    })
}

#[cfg(test)]
pub(super) fn railway_bnb_gas_fee_quote(provider_gas_price: u128) -> PublicActionGasFeeQuote {
    railway_bnb_gas_fee_quote_bundle(provider_gas_price).standard
}

pub(super) fn railway_bnb_gas_fee_quote_bundle(
    provider_gas_price: u128,
) -> PublicActionGasFeeQuoteBundle {
    let capped_gas_price = provider_gas_price.min(RAILWAY_BNB_GAS_PRICE_CAP);
    let standard_gas_price = capped_gas_price * 110 / 100;
    let aggressive_gas_price = capped_gas_price * 140 / 100;
    let standard = PublicActionGasFeeQuote {
        rpc_gas_price: standard_gas_price,
        current_base_fee_per_gas: None,
        suggested_max_fee_per_gas: standard_gas_price,
        suggested_max_priority_fee_per_gas: 0,
    };
    PublicActionGasFeeQuoteBundle {
        standard,
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: aggressive_gas_price,
            max_priority_fee_per_gas: 0,
        },
    }
}

const fn public_action_gas_fee_quote_bundle_from_standard(
    standard: PublicActionGasFeeQuote,
) -> PublicActionGasFeeQuoteBundle {
    PublicActionGasFeeQuoteBundle {
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: standard.suggested_max_fee_per_gas,
            max_priority_fee_per_gas: standard.suggested_max_priority_fee_per_gas,
        },
        standard,
    }
}

pub(super) fn railway_lower_median(values: &mut [u128]) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.get((values.len() - 1) / 2).copied()
}

pub(super) const fn public_action_tip_fallback(chain_id: u64) -> SelfBroadcastTipFallback {
    if chain_id == PUBLIC_ACTION_BNB_CHAIN_ID {
        SelfBroadcastTipFallback::RpcGasPrice
    } else {
        SelfBroadcastTipFallback::Minimum
    }
}

const fn public_native_step_gas_units(step: PublicActionProgressStep) -> u64 {
    match step {
        PublicActionProgressStep::Send => PUBLIC_NATIVE_SEND_GAS_UNITS,
        PublicActionProgressStep::Wrap => PUBLIC_NATIVE_WRAP_GAS_UNITS,
        PublicActionProgressStep::Approve => PUBLIC_NATIVE_APPROVE_GAS_UNITS,
        PublicActionProgressStep::Shield => PUBLIC_NATIVE_RELAY_ADAPT_SHIELD_GAS_UNITS,
        PublicActionProgressStep::ShieldKey
        | PublicActionProgressStep::Sponsor
        | PublicActionProgressStep::Unsponsor
        | PublicActionProgressStep::CallVote
        | PublicActionProgressStep::Vote
        | PublicActionProgressStep::GovernanceApprove
        | PublicActionProgressStep::Stake
        | PublicActionProgressStep::Delegate
        | PublicActionProgressStep::Undelegate
        | PublicActionProgressStep::Unlock
        | PublicActionProgressStep::PrincipalClaim
        | PublicActionProgressStep::RewardClaim(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undecodable_revert_data_exposes_only_its_selector() {
        assert_eq!(
            friendly_revert_data(&[0x12, 0x34, 0x56, 0x78, 0xab, 0xcd]),
            "Custom error 0x12345678"
        );
    }

    #[test]
    fn tied_provider_failure_groups_are_unavailable() {
        let error = select_provider_failure(vec![
            ProviderFailure {
                group: ProviderFailureGroup::RevertMessage("expired".to_owned()),
                reason: "Order has expired".to_owned(),
            },
            ProviderFailure {
                group: ProviderFailureGroup::JsonRpc {
                    code: -32000,
                    message: "temporarily unavailable".to_owned(),
                },
                reason: "RPC error -32000: temporarily unavailable".to_owned(),
            },
        ]);
        assert!(matches!(
            error,
            PublicAdvancedTransactionSimulationError::Unavailable(reason)
                if reason == "RPC providers returned conflicting simulation results."
        ));
    }
}
