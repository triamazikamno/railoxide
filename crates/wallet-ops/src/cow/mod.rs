//! `CoW` Protocol orderbook client for private swaps.
//!
//! Every request goes through one swap's [`OperationHttpClient`], so the orderbook sees that
//! swap's traffic on its own Tor circuits in built-in Tor mode. Request and response shapes
//! follow `cowprotocol/services` `v2.380.2` (`crates/orderbook/openapi.yml`,
//! `crates/model/src/quote.rs`, `crates/model/src/order.rs`). Order, UID, and app-data
//! encoding come from [`broadcaster_core::contracts::cow`].
//!
//! Quote requests are built only from [`CowSellQuoteRequest`], which has no field for hooks,
//! signatures, or app data. They always send the empty app-data document, so a quote can't
//! carry a signed payload. Signed hooks leave the wallet only through
//! [`CowOrderbookClient::submit_order`].
//!
//! Base URLs may carry credentials. They are never logged or formatted raw.

use std::error::Error as _;
use std::fmt;
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256};
pub use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::cow::{EncodedAppData, Order};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::http::{OperationHttpClient, OperationNetworkIsolation, redact_url_for_display};

mod pricing;
#[cfg(test)]
mod tests;

pub use pricing::{
    DeliveryAllowanceParams, DeliveryAllowanceRate, GAS_SHARE_BALANCED_BPS, GAS_SHARE_LOOSE_BPS,
    GAS_SHARE_TIGHT_BPS, NativeBuyRate, OrderLimit, OrderLimitError, OrderLimitParams,
    PreHookCalls, across_post_hook_gas, delivery_allowance, destination_minimum_after_allowance,
    hook_gas_limit, order_buy_amount, post_hook_gas, pre_hook_gas, price_order_limit,
    private_delivery_gas, public_deposit_hook_gas, quote_gas_units, quote_protocol_fee,
};

/// Hook-free app data sent with every quote request. `CoW` documents `"{}"` as the app data
/// for orders that carry no metadata.
const QUOTE_APP_DATA: &str = "{}";
const SIGNING_SCHEME_EIP712: &str = "eip712";
const QUOTE_KIND_SELL: &str = "sell";
const COW_QUOTE_TIMEOUT: Duration = Duration::from_secs(15);
const COW_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
/// Order reads return the full app data, which production accepts up to 81,920 bytes.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Description of the app-data size check in `cowprotocol/services` `v2.380.2`
/// (`crates/app-data/src/app_data.rs`, `Validator::validate`). The orderbook returns it under
/// the `InvalidAppData` error type, which also covers malformed documents.
const APP_DATA_SIZE_REJECTION: &str = "larger than limit";
/// Longest orderbook error description shown to the user.
const MAX_DESCRIPTION_CHARS: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CowApiError {
    #[error("the CoW orderbook URL can't be used as a base URL")]
    InvalidBaseUrl,
    /// The orderbook rejected the order's app data as too large. The swap should offer a
    /// smaller amount rather than fail.
    #[error("the order's app data is too large for the CoW orderbook")]
    AppDataTooLarge,
    #[error("the CoW orderbook already accepted this order")]
    DuplicatedOrder,
    #[error("the sell amount doesn't cover the CoW network fee")]
    SellAmountDoesNotCoverFee,
    #[error("CoW found no liquidity for this trade")]
    NoLiquidity,
    #[error("CoW doesn't support one of the tokens")]
    UnsupportedToken,
    #[error("the CoW orderbook rate-limited the request")]
    RateLimited,
    #[error("the CoW orderbook doesn't know this order")]
    NotFound,
    #[error(
        "the CoW orderbook rejected the request ({error_type}, HTTP {status}){}",
        description_suffix(.description)
    )]
    Rejected {
        status: u16,
        error_type: String,
        description: String,
    },
    #[error("the CoW orderbook is unavailable (HTTP {status})")]
    Unavailable { status: u16 },
    #[error("the CoW orderbook {operation} request to {endpoint} timed out")]
    Timeout {
        operation: &'static str,
        endpoint: String,
    },
    #[error("the CoW orderbook {operation} request to {endpoint} failed: {cause}")]
    Transport {
        operation: &'static str,
        endpoint: String,
        cause: String,
    },
    #[error("the CoW orderbook response is too large")]
    ResponseTooLarge,
    #[error("the CoW orderbook returned an invalid response")]
    InvalidResponse,
}

impl CowApiError {
    /// Whether the swap should return to the smaller-amount offer instead of failing.
    #[must_use]
    pub const fn requires_replan(&self) -> bool {
        matches!(self, Self::AppDataTooLarge)
    }

    /// The error without the orderbook's description, which can echo the signed request.
    fn without_description(&self) -> Self {
        match self {
            Self::Rejected {
                status, error_type, ..
            } => Self::Rejected {
                status: *status,
                error_type: error_type.clone(),
                description: String::new(),
            },
            other => other.clone(),
        }
    }
}

fn description_suffix(description: &str) -> String {
    if description.is_empty() {
        String::new()
    } else {
        format!(": {description}")
    }
}

/// Parameters of a sell-order quote. Quotes never carry hooks or signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CowSellQuoteRequest {
    pub sell_token: Address,
    pub buy_token: Address,
    /// The order owner, which for a private swap is its executor.
    pub from: Address,
    pub receiver: Address,
    pub sell_amount_before_fee: U256,
    pub valid_to: u32,
}

/// `OrderQuoteResponse`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CowQuote {
    pub quote: CowQuoteParameters,
    pub expiration: String,
    #[serde(default)]
    pub id: Option<i64>,
    pub verified: bool,
    #[serde(default)]
    pub protocol_fee_bps: Option<String>,
}

/// Quoted `OrderParameters`. Gas fields stay as the decimal strings the API returns.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CowQuoteParameters {
    pub sell_token: Address,
    pub buy_token: Address,
    #[serde(default)]
    pub receiver: Option<Address>,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub valid_to: u32,
    pub fee_amount: U256,
    pub gas_amount: String,
    pub gas_price: String,
    pub sell_token_price: String,
    pub kind: String,
    pub partially_fillable: bool,
}

/// A signed order and the full app data its `appData` field commits to. It has no `Debug`
/// so that signed hooks can't reach logs before the order is submitted.
#[derive(Clone, Copy)]
pub struct CowOrderSubmission<'a> {
    pub order: &'a Order,
    pub owner: Address,
    /// `eip712` scheme signature, `r || s || v`.
    pub signature: &'a [u8; 65],
    pub app_data: &'a EncodedAppData,
    pub quote_id: Option<i64>,
}

/// Orderbook order status. It is a hint only and never establishes a swap outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CowOrderStatusHint {
    PresignaturePending,
    Open,
    Fulfilled,
    Cancelled,
    Expired,
    #[serde(other)]
    Unknown,
}

/// The orderbook's report on one order. Every field is a hint for display and never
/// establishes a swap outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CowOrderStatusReport {
    pub status: CowOrderStatusHint,
}

/// The fee the orderbook reports it charged a filled order. It is a hint for display and
/// never establishes a swap outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CowExecutedFee {
    /// `executedFee`, in `token` base units.
    pub amount: U256,
    /// `executedFeeToken`.
    pub token: Address,
}

/// Orderbook client bound to one swap's network route.
#[derive(Clone)]
pub struct CowOrderbookClient {
    http: OperationHttpClient,
    base_url: Url,
    chain_id: u64,
}

impl fmt::Debug for CowOrderbookClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CowOrderbookClient")
            .field("http", &self.http)
            .field("base_url", &redact_url_for_display(&self.base_url))
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

impl CowOrderbookClient {
    /// `base_url` is the orderbook API root, for example `https://api.cow.fi/mainnet`.
    pub fn new(
        http: OperationHttpClient,
        base_url: Url,
        chain_id: u64,
    ) -> Result<Self, CowApiError> {
        if base_url.cannot_be_a_base() {
            return Err(CowApiError::InvalidBaseUrl);
        }
        Ok(Self {
            http,
            base_url,
            chain_id,
        })
    }

    /// Isolation of this swap's orderbook traffic, for the review screen's disclosure.
    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.http.isolation()
    }

    /// The swap's HTTP route, so its bridge clients share this client's isolation group.
    #[must_use]
    pub const fn http(&self) -> &OperationHttpClient {
        &self.http
    }

    /// `POST /api/v1/quote` for a sell order, with hook-free app data.
    pub async fn quote_sell(&self, request: &CowSellQuoteRequest) -> Result<CowQuote, CowApiError> {
        let body = QuoteRequestBody {
            from: request.from,
            sell_token: request.sell_token,
            buy_token: request.buy_token,
            receiver: request.receiver,
            kind: QUOTE_KIND_SELL,
            sell_amount_before_fee: request.sell_amount_before_fee,
            valid_to: request.valid_to,
            app_data: QUOTE_APP_DATA,
            app_data_hash: keccak256(QUOTE_APP_DATA.as_bytes()),
            signing_scheme: SIGNING_SCHEME_EIP712,
        };
        let request = self
            .http
            .client()
            .post(self.endpoint(&["api", "v1", "quote"]))
            .json(&body);
        self.execute("quote", request, COW_QUOTE_TIMEOUT).await
    }

    /// `POST /api/v1/orders`. This is the only request that carries signed hooks.
    ///
    /// Returns the UID the orderbook assigned. Callers compare it with the UID they computed.
    pub async fn submit_order(
        &self,
        submission: &CowOrderSubmission<'_>,
    ) -> Result<OrderUid, CowApiError> {
        let order = submission.order;
        let body = OrderCreationBody {
            sell_token: order.sellToken,
            buy_token: order.buyToken,
            receiver: order.receiver,
            sell_amount: order.sellAmount,
            buy_amount: order.buyAmount,
            valid_to: order.validTo,
            fee_amount: order.feeAmount,
            kind: &order.kind,
            partially_fillable: order.partiallyFillable,
            sell_token_balance: &order.sellTokenBalance,
            buy_token_balance: &order.buyTokenBalance,
            signing_scheme: SIGNING_SCHEME_EIP712,
            signature: Bytes::copy_from_slice(submission.signature),
            from: submission.owner,
            quote_id: submission.quote_id,
            app_data: &submission.app_data.document,
            app_data_hash: submission.app_data.hash,
        };
        let request = self
            .http
            .client()
            .post(self.endpoint(&["api", "v1", "orders"]))
            .json(&body);
        let uid: FixedBytes<56> = self
            .execute("order submission", request, COW_REQUEST_TIMEOUT)
            .await?;
        Ok(OrderUid(uid))
    }

    /// `GET /api/v1/orders/{uid}`. The report is a hint and never a swap outcome.
    pub async fn order_status_hint(
        &self,
        uid: &OrderUid,
    ) -> Result<CowOrderStatusReport, CowApiError> {
        let uid = uid.0.to_string();
        let request = self
            .http
            .client()
            .get(self.endpoint(&["api", "v1", "orders", &uid]));
        let order: OrderStatusBody = self
            .execute("order status", request, COW_REQUEST_TIMEOUT)
            .await?;
        Ok(CowOrderStatusReport {
            status: order.status,
        })
    }

    /// `GET /api/v1/orders/{uid}`: the fee the orderbook reports it charged the order, or
    /// `None` when the report has none. It is a hint and never a swap outcome. The request
    /// carries only the order UID.
    pub async fn order_executed_fee(
        &self,
        uid: &OrderUid,
    ) -> Result<Option<CowExecutedFee>, CowApiError> {
        let uid = uid.0.to_string();
        let request = self
            .http
            .client()
            .get(self.endpoint(&["api", "v1", "orders", &uid]));
        let order: OrderExecutedFeeBody = self
            .execute("order fee", request, COW_REQUEST_TIMEOUT)
            .await?;
        Ok(order
            .executed_fee
            .zip(order.executed_fee_token)
            .map(|(amount, token)| CowExecutedFee { amount, token }))
    }

    /// `GET /api/v1/trades?orderUid={uid}`: the block of the order's latest reported trade.
    /// It is a hint and never a swap outcome. The request carries only the order UID.
    pub async fn order_trade_block(&self, uid: &OrderUid) -> Result<Option<u64>, CowApiError> {
        let mut url = self.endpoint(&["api", "v1", "trades"]);
        url.query_pairs_mut()
            .append_pair("orderUid", &uid.0.to_string());
        let request = self.http.client().get(url);
        let trades: Vec<TradeBody> = self
            .execute("order trades", request, COW_REQUEST_TIMEOUT)
            .await?;
        Ok(trades.iter().map(|trade| trade.block_number).max())
    }

    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        // `new` rejected cannot-be-a-base URLs, so path segments are available.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    async fn execute<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, CowApiError> {
        let result = async {
            let mut response = request
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| self.request_error(operation, error))?;
            let status = response.status();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| self.request_error(operation, error))?
            {
                let size = body
                    .len()
                    .checked_add(chunk.len())
                    .ok_or(CowApiError::ResponseTooLarge)?;
                if size > MAX_RESPONSE_BYTES {
                    return Err(CowApiError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(classify_error_response(status, &body));
            }
            serde_json::from_slice(&body).map_err(|_| CowApiError::InvalidResponse)
        }
        .await;
        if let Err(error) = &result {
            tracing::debug!(
                chain_id = self.chain_id,
                operation,
                error = %error.without_description(),
                "CoW orderbook request failed"
            );
        }
        result
    }

    /// Names the operation and host, without credentials or path, so a user can check the
    /// orderbook's reachability themselves.
    fn request_error(&self, operation: &'static str, error: reqwest::Error) -> CowApiError {
        let endpoint = redact_url_for_display(&self.base_url);
        if error.is_timeout() {
            return CowApiError::Timeout {
                operation,
                endpoint,
            };
        }
        // `reqwest` puts the underlying cause, such as a proxy or TLS failure, in the source
        // chain rather than in its own message.
        let error = error.without_url();
        let mut cause = error.to_string();
        let mut source = error.source();
        while let Some(inner) = source {
            cause.push_str(": ");
            cause.push_str(&inner.to_string());
            source = inner.source();
        }
        CowApiError::Transport {
            operation,
            endpoint,
            cause,
        }
    }
}

/// Map an orderbook error response. Descriptions are kept for display but not logged, because
/// they can echo request data.
fn classify_error_response(status: StatusCode, body: &[u8]) -> CowApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        // The order endpoint's request body limit, which the full app data counts against.
        return CowApiError::AppDataTooLarge;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return CowApiError::RateLimited;
    }
    if status.is_server_error() {
        return CowApiError::Unavailable {
            status: status.as_u16(),
        };
    }
    let Ok(error) = serde_json::from_slice::<ErrorBody>(body) else {
        return if status == StatusCode::NOT_FOUND {
            CowApiError::NotFound
        } else {
            CowApiError::Rejected {
                status: status.as_u16(),
                error_type: "unknown".to_owned(),
                description: String::new(),
            }
        };
    };
    match error.error_type.as_str() {
        "InvalidAppData" if error.description.contains(APP_DATA_SIZE_REJECTION) => {
            CowApiError::AppDataTooLarge
        }
        "DuplicatedOrder" => CowApiError::DuplicatedOrder,
        "SellAmountDoesNotCoverFee" => CowApiError::SellAmountDoesNotCoverFee,
        "NoLiquidity" | "InsufficientLiquidity" => CowApiError::NoLiquidity,
        "UnsupportedToken" => CowApiError::UnsupportedToken,
        "NotFound" => CowApiError::NotFound,
        _ => CowApiError::Rejected {
            status: status.as_u16(),
            error_type: error.error_type,
            description: error
                .description
                .chars()
                .take(MAX_DESCRIPTION_CHARS)
                .collect(),
        },
    }
}

/// `OrderQuoteRequest` for a sell order priced before fees.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteRequestBody {
    from: Address,
    sell_token: Address,
    buy_token: Address,
    receiver: Address,
    kind: &'static str,
    #[serde(with = "alloy::serde::displayfromstr")]
    sell_amount_before_fee: U256,
    valid_to: u32,
    app_data: &'static str,
    app_data_hash: B256,
    signing_scheme: &'static str,
}

/// `OrderCreation` with full app data and its hash. Amounts are decimal strings, as
/// `CoW` services serialize them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderCreationBody<'a> {
    sell_token: Address,
    buy_token: Address,
    receiver: Address,
    #[serde(with = "alloy::serde::displayfromstr")]
    sell_amount: U256,
    #[serde(with = "alloy::serde::displayfromstr")]
    buy_amount: U256,
    valid_to: u32,
    #[serde(with = "alloy::serde::displayfromstr")]
    fee_amount: U256,
    kind: &'a str,
    partially_fillable: bool,
    sell_token_balance: &'a str,
    buy_token_balance: &'a str,
    signing_scheme: &'static str,
    signature: Bytes,
    from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_id: Option<i64>,
    app_data: &'a str,
    app_data_hash: B256,
}

/// The field of `Order` the status report reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderStatusBody {
    status: CowOrderStatusHint,
}

/// The fields of `Order` the executed fee reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderExecutedFeeBody {
    #[serde(default)]
    executed_fee: Option<U256>,
    #[serde(default)]
    executed_fee_token: Option<Address>,
}

/// The field of `Trade` the trade block reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeBody {
    block_number: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    error_type: String,
    #[serde(default)]
    description: String,
}
