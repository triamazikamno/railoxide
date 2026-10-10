//! Across API client: routes, fee quotes and deposit lookup.
//!
//! Shapes follow the live `app.across.to/api` responses captured on 2026-09-29.

use alloy::primitives::{Address, B256, Bytes, U256, uint};
use reqwest::Url;
use serde::Deserialize;

use super::{BridgeApi, BridgeApiError, BridgeHttp, json_uint};
use crate::http::{OperationHttpClient, OperationNetworkIsolation};

/// The scale of the API's `pct` fields: 1e18 is 100%.
const PCT_SCALE: U256 = uint!(1_000_000_000_000_000_000_U256);

/// A route an Across deposit can take from the origin chain to the destination chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcrossRoute {
    pub origin_token: Address,
    pub destination_token: Address,
    pub origin_symbol: String,
    pub destination_symbol: String,
}

/// Parameters of a fee quote. The type has no field for a recipient, message or depositor, so
/// a quote can't disclose who receives the funds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcrossFeeRequest {
    pub input_token: Address,
    pub output_token: Address,
    pub origin_chain: u64,
    pub destination_chain: u64,
    pub amount: U256,
}

/// A fee quote for an approved private-delivery order, requested only while signing it. Unlike
/// [`AcrossFeeRequest`] it names the fill's recipient, the handler, and carries the handler
/// message, so Across simulates the real fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcrossMessageFeeRequest {
    pub fee: AcrossFeeRequest,
    pub recipient: Address,
    pub message: Bytes,
}

/// The fee quote terms a `depositV3` call needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcrossFeeQuote {
    pub output_amount: U256,
    pub total_relay_fee_total: U256,
    /// The whole relay fee as a fraction of the input amount, scaled by 1e18.
    pub total_relay_fee_pct: U256,
    /// The relayer's gas fee for the fill, in input-token base units. It is part of
    /// `total_relay_fee_total`.
    pub relayer_gas_fee_total: U256,
    /// The relayer's gas fee as a fraction of the input amount, scaled by 1e18.
    pub relayer_gas_fee_pct: U256,
    pub lp_fee_total: U256,
    /// `quoteTimestamp` for the deposit.
    pub timestamp: u32,
    pub fill_deadline: u32,
    pub exclusive_relayer: Address,
    pub exclusivity_deadline: u32,
    pub spoke_pool: Address,
    pub destination_spoke_pool: Address,
    pub is_amount_too_low: bool,
    pub min_deposit: U256,
    pub max_deposit: U256,
    pub estimated_fill_time_sec: u64,
}

impl AcrossFeeQuote {
    /// The relayer's gas fee in output-token base units, rounded up:
    /// `ceil(output_amount * relayer_gas_fee_pct / (1e18 - total_relay_fee_pct))`.
    /// `relayer_gas_fee_total` is in input-token units, whose decimals can differ from the
    /// output token's, as for USDC on BNB Chain, while the output is the input less the whole
    /// relay fee, `input * (1 - total_relay_fee_pct)`. `None` when the whole fee is 100% or
    /// more, or the product overflows.
    #[must_use]
    pub fn relayer_gas_fee_in_output(&self) -> Option<U256> {
        let kept = PCT_SCALE
            .checked_sub(self.total_relay_fee_pct)
            .filter(|kept| !kept.is_zero())?;
        Some(
            self.output_amount
                .checked_mul(self.relayer_gas_fee_pct)?
                .div_ceil(kept),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcrossDepositStatus {
    Pending,
    Filled,
    Expired,
    Refunded,
    SlowFillRequested,
    #[serde(other)]
    Unknown,
}

/// Across's record of a deposit. It only locates the fill; the destination chain's receipts
/// are the evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcrossDeposit {
    pub status: AcrossDepositStatus,
    pub fill_block_number: Option<u64>,
    pub fill_tx: Option<B256>,
    pub deposit_refund_tx: Option<B256>,
    pub output_amount: U256,
    pub recipient: Address,
    pub destination_chain: u64,
}

/// Across API client bound to one swap's network route.
#[derive(Clone, Debug)]
pub struct AcrossClient {
    inner: BridgeHttp,
}

impl AcrossClient {
    /// `base_url` is the API root, for example `https://app.across.to/api`.
    pub fn new(http: OperationHttpClient, base_url: Url) -> Result<Self, BridgeApiError> {
        Ok(Self {
            inner: BridgeHttp::new(BridgeApi::Across, http, base_url)?,
        })
    }

    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.inner.isolation()
    }

    /// `GET /available-routes`. Native-asset entries are skipped: they are the `msg.value`
    /// form of an ERC-20 route, and the executor always deposits the ERC-20.
    pub async fn available_routes(
        &self,
        origin_chain: u64,
        destination_chain: u64,
    ) -> Result<Vec<AcrossRoute>, BridgeApiError> {
        let origin = origin_chain.to_string();
        let destination = destination_chain.to_string();
        let request = self.inner.get(
            &["available-routes"],
            &[
                ("originChainId", origin.as_str()),
                ("destinationChainId", destination.as_str()),
            ],
        );
        let routes: Vec<RouteBody> = self.inner.json("routes", request).await?;
        Ok(routes
            .into_iter()
            .filter(|route| {
                !route.is_native
                    && route.origin_chain_id == origin_chain
                    && route.destination_chain_id == destination_chain
            })
            .map(|route| AcrossRoute {
                origin_token: route.origin_token,
                destination_token: route.destination_token,
                origin_symbol: route.origin_token_symbol,
                destination_symbol: route.destination_token_symbol,
            })
            .collect())
    }

    /// `GET /suggested-fees` with only the tokens, chains and amount. The output amount is in
    /// the output token's units, whose decimals may differ from the input's, as for USDC and
    /// USDT on BNB Chain; Across refuses such a pair unless the request allows it.
    pub async fn suggested_fees(
        &self,
        request: &AcrossFeeRequest,
    ) -> Result<AcrossFeeQuote, BridgeApiError> {
        self.fee_quote(request, &[]).await
    }

    /// `GET /suggested-fees` with the fill's recipient and message as well. Across simulates
    /// the fill, and a message that reverts there is [`BridgeApiError::FillSimulationFailed`].
    pub async fn suggested_fees_with_message(
        &self,
        request: &AcrossMessageFeeRequest,
    ) -> Result<AcrossFeeQuote, BridgeApiError> {
        let recipient = request.recipient.to_string();
        let message = request.message.to_string();
        self.fee_quote(
            &request.fee,
            &[
                ("recipient", recipient.as_str()),
                ("message", message.as_str()),
            ],
        )
        .await
    }

    /// `GET /suggested-fees` with `extra` query pairs after the tokens, chains and amount.
    async fn fee_quote(
        &self,
        request: &AcrossFeeRequest,
        extra: &[(&str, &str)],
    ) -> Result<AcrossFeeQuote, BridgeApiError> {
        let input_token = request.input_token.to_string();
        let output_token = request.output_token.to_string();
        let origin = request.origin_chain.to_string();
        let destination = request.destination_chain.to_string();
        let amount = request.amount.to_string();
        let mut query = vec![
            ("inputToken", input_token.as_str()),
            ("outputToken", output_token.as_str()),
            ("originChainId", origin.as_str()),
            ("destinationChainId", destination.as_str()),
            ("amount", amount.as_str()),
            ("allowUnmatchedDecimals", "true"),
        ];
        query.extend_from_slice(extra);
        let request = self.inner.get(&["suggested-fees"], &query);
        let fees: SuggestedFeesBody = self.inner.json("fee quote", request).await?;
        if fees.is_amount_too_low {
            return Err(BridgeApiError::AmountTooLow);
        }
        Ok(AcrossFeeQuote {
            output_amount: fees.output_amount,
            total_relay_fee_total: fees.total_relay_fee.total,
            total_relay_fee_pct: fees.total_relay_fee.pct,
            relayer_gas_fee_total: fees.relayer_gas_fee.total,
            relayer_gas_fee_pct: fees.relayer_gas_fee.pct,
            lp_fee_total: fees.lp_fee.total,
            timestamp: fees.timestamp,
            fill_deadline: fees.fill_deadline,
            exclusive_relayer: fees.exclusive_relayer,
            exclusivity_deadline: fees.exclusivity_deadline,
            spoke_pool: fees.spoke_pool_address,
            destination_spoke_pool: fees.destination_spoke_pool_address,
            is_amount_too_low: fees.is_amount_too_low,
            min_deposit: fees.limits.min_deposit,
            max_deposit: fees.limits.max_deposit,
            estimated_fill_time_sec: fees.estimated_fill_time_sec,
        })
    }

    /// `GET /deposit` by origin chain and deposit id. `None` while Across hasn't indexed it.
    pub async fn deposit(
        &self,
        origin_chain: u64,
        deposit_id: U256,
    ) -> Result<Option<AcrossDeposit>, BridgeApiError> {
        let origin = origin_chain.to_string();
        let deposit_id = deposit_id.to_string();
        let request = self.inner.get(
            &["deposit"],
            &[
                ("originChainId", origin.as_str()),
                ("depositId", deposit_id.as_str()),
            ],
        );
        let body: DepositResponseBody = match self.inner.json("deposit", request).await {
            Ok(body) => body,
            Err(BridgeApiError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let deposit = body.deposit;
        Ok(Some(AcrossDeposit {
            status: deposit.status,
            fill_block_number: deposit.fill_block_number,
            fill_tx: deposit.fill_tx,
            deposit_refund_tx: deposit.deposit_refund_tx_hash,
            output_amount: deposit.output_amount,
            recipient: deposit.recipient,
            destination_chain: deposit.destination_chain_id,
        }))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteBody {
    origin_chain_id: u64,
    origin_token: Address,
    destination_chain_id: u64,
    destination_token: Address,
    origin_token_symbol: String,
    destination_token_symbol: String,
    #[serde(default)]
    is_native: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuggestedFeesBody {
    output_amount: U256,
    total_relay_fee: FeeBody,
    relayer_gas_fee: FeeBody,
    lp_fee: FeeBody,
    #[serde(deserialize_with = "json_uint")]
    timestamp: u32,
    #[serde(deserialize_with = "json_uint")]
    fill_deadline: u32,
    exclusive_relayer: Address,
    #[serde(deserialize_with = "json_uint")]
    exclusivity_deadline: u32,
    spoke_pool_address: Address,
    destination_spoke_pool_address: Address,
    is_amount_too_low: bool,
    limits: LimitsBody,
    #[serde(deserialize_with = "json_uint")]
    estimated_fill_time_sec: u64,
}

#[derive(Deserialize)]
struct FeeBody {
    /// A fraction of the input amount, scaled by 1e18.
    pct: U256,
    total: U256,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitsBody {
    min_deposit: U256,
    max_deposit: U256,
}

#[derive(Deserialize)]
struct DepositResponseBody {
    deposit: DepositBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DepositBody {
    status: AcrossDepositStatus,
    #[serde(default)]
    fill_block_number: Option<u64>,
    #[serde(default)]
    fill_tx: Option<B256>,
    #[serde(default)]
    deposit_refund_tx_hash: Option<B256>,
    output_amount: U256,
    recipient: Address,
    #[serde(deserialize_with = "json_uint")]
    destination_chain_id: u64,
}
