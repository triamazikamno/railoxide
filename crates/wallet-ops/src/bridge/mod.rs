//! Cross-chain delivery providers for private swaps: Across and NEAR Intents 1Click.
//!
//! Every request goes through one swap's [`OperationHttpClient`], the same isolation group as
//! that swap's `CoW` orderbook client (see [`crate::cow::CowOrderbookClient::http`]). Neither
//! API needs a key.
//!
//! Preview requests never carry the swap's receiver or its stealth account: Across fee quotes
//! have no recipient, message or depositor field, and 1Click dry quotes send fresh random
//! placeholder addresses. Only [`NearIntentsClient::quote`], called while signing an approved
//! order, sends the real receiver.
//!
//! Base URLs are never logged or formatted raw. Failures are logged with the provider, the
//! operation and the error only, never a URL, request body, receiver or deposit address.

use std::error::Error as _;
use std::fmt;
use std::time::Duration;

use alloy::primitives::U256;
use reqwest::{StatusCode, Url};
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};

use crate::http::{OperationHttpClient, OperationNetworkIsolation, redact_url_for_display};

mod across;
mod near_intents;
#[cfg(test)]
mod tests;
mod tokens;

pub use across::{
    AcrossClient, AcrossDeposit, AcrossDepositStatus, AcrossFeeQuote, AcrossFeeRequest, AcrossRoute,
};
#[cfg(test)]
pub(crate) use near_intents::sign_quote_response_for_tests;
pub use near_intents::{
    NearIntentsClient, OneClickDryQuote, OneClickExecutionStatus, OneClickQuoteParams,
    OneClickStatus, OneClickToken, OneClickVerifyError, VerifiedOneClickQuote,
};
#[cfg(feature = "test-support")]
pub use near_intents::{sign_quote_response_with_stand_in, stand_in_quote_key};
pub use tokens::{
    BridgeDestination, NearAssets, across_destination_tokens, near_destination_tokens,
};

const BRIDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
/// Longest provider error message shown to the user.
const MAX_MESSAGE_CHARS: usize = 300;

/// The provider an API error came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeApi {
    Across,
    NearIntents,
}

impl fmt::Display for BridgeApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Across => "Across",
            Self::NearIntents => "NEAR Intents",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BridgeApiError {
    #[error("the {api} API URL can't be used as a base URL")]
    InvalidBaseUrl { api: BridgeApi },
    #[error("the NEAR Intents quote-signing key is invalid")]
    InvalidQuoteKey,
    #[error("the system random number generator failed")]
    Randomness,
    #[error("{api} rate-limited the request")]
    RateLimited { api: BridgeApi },
    #[error("{api} is unavailable (HTTP {status})")]
    Unavailable { api: BridgeApi, status: u16 },
    #[error("the {api} {operation} request to {endpoint} timed out")]
    Timeout {
        api: BridgeApi,
        operation: &'static str,
        endpoint: String,
    },
    #[error("the {api} {operation} request to {endpoint} failed: {cause}")]
    Transport {
        api: BridgeApi,
        operation: &'static str,
        endpoint: String,
        cause: String,
    },
    #[error("the {api} response is too large")]
    ResponseTooLarge { api: BridgeApi },
    #[error("{api} returned an invalid response")]
    InvalidResponse { api: BridgeApi },
    #[error("{api} doesn't know this transfer")]
    NotFound { api: BridgeApi },
    #[error("the amount is too small for Across")]
    AmountTooLow,
    #[error("{api} rejected the request ({code}, HTTP {status}){}", message_suffix(.message))]
    Rejected {
        api: BridgeApi,
        status: u16,
        code: String,
        message: String,
    },
    #[error("NEAR Intents' quote couldn't be verified: {0}")]
    QuoteUnverified(OneClickVerifyError),
}

impl BridgeApiError {
    /// The error without the provider's message, which can echo request data.
    fn without_message(&self) -> Self {
        match self {
            Self::Rejected {
                api, status, code, ..
            } => Self::Rejected {
                api: *api,
                status: *status,
                code: code.clone(),
                message: String::new(),
            },
            other => other.clone(),
        }
    }
}

fn message_suffix(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

/// One provider's API root on one swap's network route.
#[derive(Clone)]
struct BridgeHttp {
    api: BridgeApi,
    http: OperationHttpClient,
    base_url: Url,
}

impl fmt::Debug for BridgeHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeHttp")
            .field("api", &self.api)
            .field("http", &self.http)
            .field("base_url", &redact_url_for_display(&self.base_url))
            .finish()
    }
}

impl BridgeHttp {
    fn new(
        api: BridgeApi,
        http: OperationHttpClient,
        base_url: Url,
    ) -> Result<Self, BridgeApiError> {
        if base_url.cannot_be_a_base() {
            return Err(BridgeApiError::InvalidBaseUrl { api });
        }
        Ok(Self {
            api,
            http,
            base_url,
        })
    }

    const fn isolation(&self) -> OperationNetworkIsolation {
        self.http.isolation()
    }

    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        // `new` rejected cannot-be-a-base URLs, so path segments are available.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    fn get(&self, segments: &[&str], query: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let mut url = self.endpoint(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.http.client().get(url)
    }

    fn post(&self, segments: &[&str]) -> reqwest::RequestBuilder {
        self.http.client().post(self.endpoint(segments))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, BridgeApiError> {
        let api = self.api;
        self.execute(operation, request, |body| {
            serde_json::from_slice(&body).map_err(|_| BridgeApiError::InvalidResponse { api })
        })
        .await
    }

    /// Send `request`, read a bounded body, classify error statuses and `parse` a success
    /// body. Failures are logged without the URL, request or response body.
    async fn execute<T>(
        &self,
        operation: &'static str,
        request: reqwest::RequestBuilder,
        parse: impl FnOnce(Vec<u8>) -> Result<T, BridgeApiError>,
    ) -> Result<T, BridgeApiError> {
        let result = async {
            let mut response = request
                .timeout(BRIDGE_REQUEST_TIMEOUT)
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
                    .ok_or(BridgeApiError::ResponseTooLarge { api: self.api })?;
                if size > MAX_RESPONSE_BYTES {
                    return Err(BridgeApiError::ResponseTooLarge { api: self.api });
                }
                body.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(classify_error_response(self.api, status, &body));
            }
            parse(body)
        }
        .await;
        if let Err(error) = &result {
            tracing::debug!(
                provider = %self.api,
                operation,
                error = %error.without_message(),
                "bridge API request failed"
            );
        }
        result
    }

    /// Names the operation and host, without credentials or path, so a user can check the
    /// provider's reachability themselves.
    fn request_error(&self, operation: &'static str, error: reqwest::Error) -> BridgeApiError {
        let api = self.api;
        let endpoint = redact_url_for_display(&self.base_url);
        if error.is_timeout() {
            return BridgeApiError::Timeout {
                api,
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
        BridgeApiError::Transport {
            api,
            operation,
            endpoint,
            cause,
        }
    }
}

/// Map a provider error response. Messages are kept for display but not logged, because they
/// can echo request data.
fn classify_error_response(api: BridgeApi, status: StatusCode, body: &[u8]) -> BridgeApiError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return BridgeApiError::RateLimited { api };
    }
    if status.is_server_error() {
        return BridgeApiError::Unavailable {
            api,
            status: status.as_u16(),
        };
    }
    let error = serde_json::from_slice::<ErrorBody>(body).unwrap_or_default();
    let code = error.code.or(error.error);
    match (api, code.as_deref()) {
        (BridgeApi::Across, Some("AMOUNT_TOO_LOW")) => return BridgeApiError::AmountTooLow,
        // Across answers 404 for unknown paths too; only this code means "no such deposit".
        (BridgeApi::Across, Some("DepositNotFoundException")) | (BridgeApi::NearIntents, _)
            if status == StatusCode::NOT_FOUND =>
        {
            return BridgeApiError::NotFound { api };
        }
        _ => {}
    }
    BridgeApiError::Rejected {
        api,
        status: status.as_u16(),
        code: code.unwrap_or_else(|| "unknown".to_owned()),
        message: error
            .message
            .unwrap_or_default()
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect(),
    }
}

/// Across's `{"type", "code", "status", "message"}` and `{"error", "message"}` bodies, and
/// 1Click's `{"message"}`.
#[derive(Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// An unsigned integer that the APIs send as a JSON number or a decimal string, depending on
/// the field. Alloy's `displayfromstr` accepts only strings, so parse through [`U256`], whose
/// serde implementation accepts both.
fn json_uint<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<U256>,
{
    let value = U256::deserialize(deserializer)?;
    T::try_from(value).map_err(|_| D::Error::custom("integer out of range"))
}
