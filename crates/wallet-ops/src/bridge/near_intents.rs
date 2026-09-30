//! NEAR Intents 1Click API client: token list, quotes and execution status.
//!
//! Quotes are `FLEX_INPUT` swaps deposited and refunded on the origin chain. Dry quotes, used
//! for previews, send a fresh OS-random recipient and refund address with every request, so
//! neither the receiver nor the swap's stealth account leaves the wallet before approval.
//! Every quote response is verified against 1Click's pinned key before use (see
//! [`verify_quote`]).

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use alloy::primitives::{Address, U256};
use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use super::{BridgeApi, BridgeApiError, BridgeHttp};
use crate::http::{OperationHttpClient, OperationNetworkIsolation};

const ED25519_PREFIX: &str = "ed25519:";
const SWAP_TYPE: &str = "FLEX_INPUT";
const DEPOSIT_TYPE: &str = "ORIGIN_CHAIN";
const REFUND_TYPE: &str = "ORIGIN_CHAIN";
const RECIPIENT_TYPE: &str = "DESTINATION_CHAIN";

// The signed fields follow `buildSignedQuoteRequest` and `buildSignedQuote` in 1Click's
// TypeScript SDK 0.1.26 (`src/quote-signature.ts`). JavaScript's `undefined` drops a key from
// the signed JSON, while `null` stays.

/// `quoteRequest` fields the signature covers as returned.
const SIGNED_REQUEST_FIELDS: [&str; 12] = [
    "dry",
    "swapType",
    "slippageTolerance",
    "originAsset",
    "depositType",
    "destinationAsset",
    "amount",
    "refundTo",
    "refundType",
    "recipient",
    "recipientType",
    "deadline",
];
/// `quoteRequest` fields the signature covers only when JavaScript-truthy.
const SIGNED_TRUTHY_REQUEST_FIELDS: [&str; 5] = [
    "quoteWaitingTimeMs",
    "referral",
    "virtualChainRecipient",
    "virtualChainRefundRecipient",
    "customRecipientMsg",
];
/// `quote` fields the signature covers as returned.
const SIGNED_QUOTE_FIELDS: [&str; 8] = [
    "amountIn",
    "amountInFormatted",
    "amountInUsd",
    "minAmountIn",
    "amountOut",
    "amountOutFormatted",
    "amountOutUsd",
    "minAmountOut",
];
/// `quote` fields of a non-dry quote that the signature covers when truthy. When falsy they
/// remove the key, including the request's `deadline`.
const SIGNED_TRUTHY_QUOTE_FIELDS: [&str; 7] = [
    "depositAddress",
    "depositMemo",
    "deadline",
    "timeWhenInactive",
    "timeEstimate",
    "refundFee",
    "withdrawFee",
];

/// Why a 1Click quote response was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OneClickVerifyError {
    #[error("the response isn't a signed quote")]
    Malformed,
    #[error("its signature doesn't match 1Click's key")]
    Signature,
    #[error("its signed {0} differs from the request")]
    TermsMismatch(&'static str),
    #[error("its deposit address can't receive the swap")]
    DepositAddress,
}

/// A token 1Click can swap.
#[derive(Clone, Debug, PartialEq)]
pub struct OneClickToken {
    pub asset_id: String,
    pub blockchain: String,
    pub symbol: String,
    pub decimals: u8,
    /// The EVM contract. `None` for native assets and non-EVM contract ids.
    pub contract_address: Option<Address>,
    /// USD price, for display only.
    pub price: Option<f64>,
}

/// Terms of a quote request. The type has no recipient or refund field: dry quotes send
/// random placeholders, and [`NearIntentsClient::quote`] takes the real ones explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneClickQuoteParams {
    pub origin_asset: String,
    pub destination_asset: String,
    pub amount: U256,
    pub slippage_bps: u32,
    pub deadline: SystemTime,
}

/// A verified preview quote. It has no deposit address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneClickDryQuote {
    pub amount_in: U256,
    pub min_amount_in: U256,
    pub amount_out: U256,
    pub min_amount_out: U256,
    pub time_estimate_sec: Option<u64>,
}

/// A verified signing-time quote.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedOneClickQuote {
    pub deposit_address: Address,
    pub amount_in: U256,
    pub min_amount_in: U256,
    pub amount_out: U256,
    pub min_amount_out: U256,
    /// The signed `quote.deadline`, as returned.
    pub deadline: String,
    /// The exact response body, which holds the request, quote, signature and timestamp, so
    /// the quote can be persisted and verified again later.
    pub signed_response: String,
}

impl fmt::Debug for VerifiedOneClickQuote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedOneClickQuote")
            .field("amount_in", &self.amount_in)
            .field("min_amount_in", &self.min_amount_in)
            .field("amount_out", &self.amount_out)
            .field("min_amount_out", &self.min_amount_out)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OneClickExecutionStatus {
    KnownDepositTx,
    PendingDeposit,
    IncompleteDeposit,
    Processing,
    Success,
    Refunded,
    Failed,
    #[serde(other)]
    Unknown,
}

/// 1Click's report on a deposit address. It is not verified on the destination chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneClickStatus {
    pub status: OneClickExecutionStatus,
    pub amount_out: Option<U256>,
    pub refunded_amount: Option<U256>,
    pub destination_tx_hashes: Vec<String>,
}

/// 1Click API client bound to one swap's network route.
#[derive(Clone)]
pub struct NearIntentsClient {
    inner: BridgeHttp,
    quote_key: VerifyingKey,
}

impl fmt::Debug for NearIntentsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NearIntentsClient")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl NearIntentsClient {
    /// `base_url` is the API root, for example `https://1click.chaindefuser.com`.
    /// `quote_key` is the `ed25519:`-prefixed base58 key that signs quotes.
    pub fn new(
        http: OperationHttpClient,
        base_url: Url,
        quote_key: &str,
    ) -> Result<Self, BridgeApiError> {
        let quote_key = parse_quote_key(quote_key).ok_or(BridgeApiError::InvalidQuoteKey)?;
        Ok(Self {
            inner: BridgeHttp::new(BridgeApi::NearIntents, http, base_url)?,
            quote_key,
        })
    }

    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.inner.isolation()
    }

    /// `GET /v0/tokens`.
    pub async fn tokens(&self) -> Result<Vec<OneClickToken>, BridgeApiError> {
        let request = self.inner.get(&["v0", "tokens"], &[]);
        let tokens: Vec<TokenBody> = self.inner.json("tokens", request).await?;
        Ok(tokens
            .into_iter()
            .map(|token| OneClickToken {
                asset_id: token.asset_id,
                blockchain: token.blockchain,
                symbol: token.symbol,
                decimals: token.decimals,
                contract_address: token
                    .contract_address
                    .and_then(|contract| contract.parse().ok()),
                price: token.price,
            })
            .collect())
    }

    /// `POST /v0/quote` with `dry: true` and fresh random placeholder recipient and refund
    /// addresses. No deposit address is created.
    pub async fn dry_quote(
        &self,
        params: &OneClickQuoteParams,
    ) -> Result<OneClickDryQuote, BridgeApiError> {
        let sent = SentQuote::new(
            params,
            true,
            random_placeholder()?,
            random_placeholder()?,
            None,
        );
        let (terms, _) = self.request_quote("dry quote", &sent).await?;
        Ok(OneClickDryQuote {
            amount_in: terms.amount_in,
            min_amount_in: terms.min_amount_in,
            amount_out: terms.amount_out,
            min_amount_out: terms.min_amount_out,
            time_estimate_sec: terms.time_estimate_sec,
        })
    }

    /// `POST /v0/quote` with `dry: false`, which creates a deposit address that pays
    /// `recipient`. Only for signing an approved order.
    pub async fn quote(
        &self,
        params: &OneClickQuoteParams,
        recipient: Address,
        refund_to: Address,
        executor: Address,
    ) -> Result<VerifiedOneClickQuote, BridgeApiError> {
        let sent = SentQuote::new(params, false, recipient, refund_to, Some(executor));
        let (terms, signed_response) = self.request_quote("quote", &sent).await?;
        let Some((deposit_address, deadline)) = terms.deposit else {
            return Err(BridgeApiError::QuoteUnverified(
                OneClickVerifyError::DepositAddress,
            ));
        };
        Ok(VerifiedOneClickQuote {
            deposit_address,
            amount_in: terms.amount_in,
            min_amount_in: terms.min_amount_in,
            amount_out: terms.amount_out,
            min_amount_out: terms.min_amount_out,
            deadline,
            signed_response,
        })
    }

    /// `GET /v0/status` by deposit address.
    pub async fn status(&self, deposit_address: Address) -> Result<OneClickStatus, BridgeApiError> {
        let deposit_address = deposit_address.to_string();
        let request = self.inner.get(
            &["v0", "status"],
            &[("depositAddress", deposit_address.as_str())],
        );
        let body: StatusBody = self.inner.json("status", request).await?;
        let details = body.swap_details.unwrap_or_default();
        Ok(OneClickStatus {
            status: body.status,
            amount_out: details.amount_out,
            refunded_amount: details.refunded_amount,
            destination_tx_hashes: details
                .destination_chain_tx_hashes
                .unwrap_or_default()
                .into_iter()
                .map(|entry| match entry {
                    TxHashBody::Hash(hash) | TxHashBody::Object { hash } => hash,
                })
                .collect(),
        })
    }

    async fn request_quote(
        &self,
        operation: &'static str,
        sent: &SentQuote<'_>,
    ) -> Result<(QuoteTerms, String), BridgeApiError> {
        let request_body = QuoteRequestBody {
            dry: sent.dry,
            swap_type: SWAP_TYPE,
            slippage_tolerance: sent.params.slippage_bps,
            origin_asset: &sent.params.origin_asset,
            deposit_type: DEPOSIT_TYPE,
            destination_asset: &sent.params.destination_asset,
            amount: sent.params.amount,
            refund_to: sent.refund_to,
            refund_type: REFUND_TYPE,
            recipient: sent.recipient,
            recipient_type: RECIPIENT_TYPE,
            deadline: &sent.deadline,
        };
        let request = self.inner.post(&["v0", "quote"]).json(&request_body);
        self.inner
            .execute(operation, request, |body| {
                let body =
                    String::from_utf8(body).map_err(|_| BridgeApiError::InvalidResponse {
                        api: BridgeApi::NearIntents,
                    })?;
                let terms = verify_quote(&body, &self.quote_key, sent)
                    .map_err(BridgeApiError::QuoteUnverified)?;
                Ok((terms, body))
            })
            .await
    }
}

/// What a quote request sent, to check the signed terms against.
pub(super) struct SentQuote<'a> {
    params: &'a OneClickQuoteParams,
    dry: bool,
    recipient: Address,
    refund_to: Address,
    /// The swap's stealth account, which must not be the deposit address.
    executor: Option<Address>,
    /// `params.deadline` as sent: ISO-8601 UTC with milliseconds.
    deadline: String,
}

impl<'a> SentQuote<'a> {
    pub(super) fn new(
        params: &'a OneClickQuoteParams,
        dry: bool,
        recipient: Address,
        refund_to: Address,
        executor: Option<Address>,
    ) -> Self {
        Self {
            params,
            dry,
            recipient,
            refund_to,
            executor,
            deadline: DateTime::<Utc>::from(params.deadline)
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        }
    }
}

/// The verified terms of a quote. `deposit` is the deposit address and signed deadline of a
/// non-dry quote.
pub(super) struct QuoteTerms {
    pub(super) amount_in: U256,
    pub(super) min_amount_in: U256,
    pub(super) amount_out: U256,
    pub(super) min_amount_out: U256,
    pub(super) time_estimate_sec: Option<u64>,
    pub(super) deposit: Option<(Address, String)>,
}

/// Verify a 1Click quote response as the SDK's `verifyQuoteSignature` does, then check the
/// signed terms against what was sent. Fails closed.
///
/// The signature covers the base58 sha256 digest of the key-sorted compact JSON of the signed
/// request fields, the signed quote fields and the response `timestamp`.
pub(super) fn verify_quote(
    body: &str,
    key: &VerifyingKey,
    sent: &SentQuote<'_>,
) -> Result<QuoteTerms, OneClickVerifyError> {
    let response: Value = serde_json::from_str(body).map_err(|_| OneClickVerifyError::Malformed)?;
    let request = response
        .get("quoteRequest")
        .and_then(Value::as_object)
        .ok_or(OneClickVerifyError::Malformed)?;
    let quote = response
        .get("quote")
        .and_then(Value::as_object)
        .ok_or(OneClickVerifyError::Malformed)?;
    let signature = response
        .get("signature")
        .and_then(Value::as_str)
        .ok_or(OneClickVerifyError::Malformed)?;
    let signature = decode_ed25519(signature)
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(OneClickVerifyError::Signature)?;
    let message = signed_message(request, quote, response.get("timestamp"))?;
    key.verify(message.as_bytes(), &signature)
        .map_err(|_| OneClickVerifyError::Signature)?;
    check_terms(request, quote, sent)
}

/// `hashQuote`: base58 of the sha256 of the stable-stringified signed fields.
pub(super) fn signed_message(
    request: &Map<String, Value>,
    quote: &Map<String, Value>,
    timestamp: Option<&Value>,
) -> Result<String, OneClickVerifyError> {
    // A `BTreeMap` sorts keys whether or not `serde_json` preserves insertion order.
    let mut signed = BTreeMap::<&str, &Value>::new();
    for field in SIGNED_REQUEST_FIELDS {
        if let Some(value) = request.get(field) {
            signed.insert(field, value);
        }
    }
    for field in SIGNED_TRUTHY_REQUEST_FIELDS {
        if let Some(value) = request.get(field).filter(|value| is_truthy(value)) {
            signed.insert(field, value);
        }
    }
    for field in SIGNED_QUOTE_FIELDS {
        if let Some(value) = quote.get(field) {
            signed.insert(field, value);
        }
    }
    if !request.get("dry").is_some_and(is_truthy) {
        for field in SIGNED_TRUTHY_QUOTE_FIELDS {
            match quote.get(field).filter(|value| is_truthy(value)) {
                Some(value) => signed.insert(field, value),
                None => signed.remove(field),
            };
        }
    }
    if let Some(timestamp) = timestamp {
        signed.insert("timestamp", timestamp);
    }
    let json = serde_json::to_string(&signed).map_err(|_| OneClickVerifyError::Malformed)?;
    Ok(bs58::encode(Sha256::digest(json.as_bytes()).as_slice()).into_string())
}

/// Sign a quote response the way 1Click does, with `key` in place of 1Click's.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn sign_quote_response_for_tests(
    mut response: Value,
    key: &ed25519_dalek::SigningKey,
) -> String {
    use ed25519_dalek::Signer as _;

    let message = signed_message(
        response["quoteRequest"].as_object().unwrap(),
        response["quote"].as_object().unwrap(),
        response.get("timestamp"),
    )
    .unwrap();
    let signature = key.sign(message.as_bytes());
    response["signature"] = Value::from(format!(
        "{ED25519_PREFIX}{}",
        bs58::encode(signature.to_bytes()).into_string()
    ));
    response.to_string()
}

/// The `ed25519:` key of the stand-in for 1Click's quote signer, `seed` repeated, for a
/// [`NearIntentsClient`] in the desktop wallet's UI tests.
#[cfg(feature = "test-support")]
#[must_use]
pub fn stand_in_quote_key(seed: u8) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    format!(
        "{ED25519_PREFIX}{}",
        bs58::encode(key.verifying_key().to_bytes()).into_string()
    )
}

/// `response` signed as 1Click signs quotes, by the stand-in [`stand_in_quote_key`] names.
#[cfg(feature = "test-support")]
#[must_use]
pub fn sign_quote_response_with_stand_in(response: Value, seed: u8) -> String {
    sign_quote_response_for_tests(
        response,
        &ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
    )
}

/// JavaScript truthiness of a JSON value.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number.abs() > 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn check_terms(
    request: &Map<String, Value>,
    quote: &Map<String, Value>,
    sent: &SentQuote<'_>,
) -> Result<QuoteTerms, OneClickVerifyError> {
    let text = |fields: &Map<String, Value>, field: &str| {
        fields.get(field).and_then(Value::as_str).map(str::to_owned)
    };
    let expect = |field: &'static str, matches: bool| {
        if matches {
            Ok(())
        } else {
            Err(OneClickVerifyError::TermsMismatch(field))
        }
    };
    let address =
        |field: &str| text(request, field).and_then(|value| value.parse::<Address>().ok());
    let time = |value: Option<String>| {
        value.and_then(|value| DateTime::<FixedOffset>::parse_from_rfc3339(&value).ok())
    };

    expect(
        "dry",
        request.get("dry").and_then(Value::as_bool) == Some(sent.dry),
    )?;
    for (field, value) in [
        ("swapType", SWAP_TYPE),
        ("originAsset", sent.params.origin_asset.as_str()),
        ("destinationAsset", sent.params.destination_asset.as_str()),
        ("depositType", DEPOSIT_TYPE),
        ("refundType", REFUND_TYPE),
        ("recipientType", RECIPIENT_TYPE),
    ] {
        expect(field, text(request, field).as_deref() == Some(value))?;
    }
    expect(
        "amount",
        text(request, "amount").and_then(|amount| amount.parse::<U256>().ok())
            == Some(sent.params.amount),
    )?;
    expect(
        "slippageTolerance",
        request.get("slippageTolerance").and_then(Value::as_u64)
            == Some(u64::from(sent.params.slippage_bps)),
    )?;
    expect("recipient", address("recipient") == Some(sent.recipient))?;
    expect("refundTo", address("refundTo") == Some(sent.refund_to))?;
    // The request's deadline is signed only for a dry quote. A non-dry quote signs its own
    // `quote.deadline` instead, which bounds how long the deposit address stays active.
    let sent_deadline = DateTime::<FixedOffset>::parse_from_rfc3339(&sent.deadline)
        .map_err(|_| OneClickVerifyError::Malformed)?;
    expect(
        "deadline",
        time(text(request, "deadline")) == Some(sent_deadline),
    )?;

    let deposit = if sent.dry {
        None
    } else {
        let deadline = text(quote, "deadline");
        expect(
            "quote deadline",
            time(deadline.clone()).is_some_and(|deadline| deadline >= sent_deadline),
        )?;
        let deposit_address = text(quote, "depositAddress")
            .and_then(|value| value.parse::<Address>().ok())
            .filter(|address| {
                *address != Address::ZERO
                    && *address != sent.recipient
                    && Some(*address) != sent.executor
            })
            .ok_or(OneClickVerifyError::DepositAddress)?;
        deadline.map(|deadline| (deposit_address, deadline))
    };

    let amount = |field: &str| {
        text(quote, field)
            .and_then(|value| value.parse::<U256>().ok())
            .ok_or(OneClickVerifyError::Malformed)
    };
    Ok(QuoteTerms {
        amount_in: amount("amountIn")?,
        min_amount_in: amount("minAmountIn")?,
        amount_out: amount("amountOut")?,
        min_amount_out: amount("minAmountOut")?,
        time_estimate_sec: quote.get("timeEstimate").and_then(Value::as_u64),
        deposit,
    })
}

fn decode_ed25519(value: &str) -> Option<Vec<u8>> {
    let encoded = value.strip_prefix(ED25519_PREFIX).unwrap_or(value);
    bs58::decode(encoded).into_vec().ok()
}

pub(super) fn parse_quote_key(value: &str) -> Option<VerifyingKey> {
    let bytes = bs58::decode(value.strip_prefix(ED25519_PREFIX)?)
        .into_vec()
        .ok()?;
    VerifyingKey::from_bytes(&bytes.try_into().ok()?).ok()
}

/// A fresh address from the OS RNG, for one dry quote only.
fn random_placeholder() -> Result<Address, BridgeApiError> {
    let mut bytes = [0_u8; 20];
    getrandom::fill(&mut bytes).map_err(|_| BridgeApiError::Randomness)?;
    Ok(Address::from(bytes))
}

/// `QuoteRequest` with origin-chain deposit and refund.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteRequestBody<'a> {
    dry: bool,
    swap_type: &'static str,
    slippage_tolerance: u32,
    origin_asset: &'a str,
    deposit_type: &'static str,
    destination_asset: &'a str,
    #[serde(with = "alloy::serde::displayfromstr")]
    amount: U256,
    refund_to: Address,
    refund_type: &'static str,
    recipient: Address,
    recipient_type: &'static str,
    deadline: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenBody {
    asset_id: String,
    decimals: u8,
    blockchain: String,
    symbol: String,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    contract_address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    status: OneClickExecutionStatus,
    #[serde(default)]
    swap_details: Option<SwapDetailsBody>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapDetailsBody {
    #[serde(default)]
    amount_out: Option<U256>,
    #[serde(default)]
    refunded_amount: Option<U256>,
    #[serde(default)]
    destination_chain_tx_hashes: Option<Vec<TxHashBody>>,
}

/// A destination transaction, as a bare hash or `{hash, explorerUrl}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum TxHashBody {
    Hash(String),
    Object { hash: String },
}
