use std::collections::VecDeque;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::hex;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::{SinkExt as _, TryStreamExt as _};
use getrandom::fill;
use reqwest_websocket::{Message, Upgrade as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::time::{Duration, Instant, timeout};
use zeroize::Zeroize;

use crate::HttpContext;

use super::{Result, WalletConnectError};

pub const WALLETCONNECT_RELAY_URL: &str = "wss://relay.walletconnect.org";
pub const WALLETCONNECT_RELAY_RPC_URL: &str = WALLETCONNECT_RELAY_URL;
pub const WALLETCONNECT_DEFAULT_PROJECT_ID: &str = "38fa3e7f14b2bd026b3031c5fa85dd09";
const WALLETCONNECT_RELAY_AUTH_TTL_SECS: u64 = 86_400;
const WALLETCONNECT_RELAY_RESPONSE_TIMEOUT_SECS: u64 = 30;
const WALLETCONNECT_RELAY_SUBSCRIPTION_IDLE_WAIT_MILLIS: u64 = 250;
const WALLETCONNECT_RELAY_SDK_VERSION: &str = env!("CARGO_PKG_VERSION");
const WALLETCONNECT_RELAY_PROTOCOL: &str = "wc";
const WALLETCONNECT_RELAY_PROTOCOL_VERSION: u8 = 2;
const WALLETCONNECT_RELAY_TOPIC_LOG_PREFIX: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectRelayConfig {
    pub project_id: String,
}

impl Default for WalletConnectRelayConfig {
    fn default() -> Self {
        Self {
            project_id: WALLETCONNECT_DEFAULT_PROJECT_ID.to_owned(),
        }
    }
}

struct RedactedSecret;

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize)]
#[zeroize(drop)]
pub struct WalletConnectRelayClientAuth {
    pub signing_key: [u8; 32],
    pub client_id: String,
}

impl fmt::Debug for WalletConnectRelayClientAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletConnectRelayClientAuth")
            .field("signing_key", &RedactedSecret)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl WalletConnectRelayClientAuth {
    pub fn random() -> Result<Self> {
        let mut signing_key = [0u8; 32];
        fill(&mut signing_key).map_err(|_| WalletConnectError::Crypto)?;
        Ok(Self::from_signing_key(signing_key))
    }

    #[must_use]
    pub fn from_signing_key(signing_key: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&signing_key);
        let mut did_key_bytes = Vec::with_capacity(34);
        did_key_bytes.extend_from_slice(&[0xed, 0x01]);
        did_key_bytes.extend_from_slice(&key.verifying_key().to_bytes());
        let client_id = format!("did:key:z{}", bs58::encode(did_key_bytes).into_string());
        Self {
            signing_key,
            client_id,
        }
    }

    pub fn client_auth_jwt(&self, audience: &str, ttl_seconds: u64) -> Result<String> {
        let now = current_unix_seconds();
        let mut nonce = [0u8; 16];
        fill(&mut nonce).map_err(|_| WalletConnectError::Crypto)?;
        let header = json!({
            "alg": "EdDSA",
            "typ": "JWT",
        });
        let claims = json!({
            "iss": self.client_id,
            "sub": hex::encode(nonce),
            "aud": audience,
            "iat": now,
            "exp": now.saturating_add(ttl_seconds),
            "act": "client_auth",
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?),
        );
        let signing_key = SigningKey::from_bytes(&self.signing_key);
        let signature = signing_key.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    pub fn bearer_token(&self, audience: &str, ttl_seconds: u64) -> Result<String> {
        self.client_auth_jwt(audience, ttl_seconds)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectJsonRpcRequest<P = Value, I = u64> {
    pub id: I,
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

impl<P> WalletConnectJsonRpcRequest<P> {
    #[must_use]
    pub fn new(id: u64, method: impl Into<String>, params: P) -> Self {
        Self {
            id,
            jsonrpc: "2.0".to_owned(),
            method: method.into(),
            params,
        }
    }
}

pub type WalletConnectRelayJsonRpcResponse<R = Value> =
    WalletConnectJsonRpcResponse<R, WalletConnectJsonRpcId>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WalletConnectJsonRpcId {
    Number(u64),
    String(String),
}

impl From<u64> for WalletConnectJsonRpcId {
    fn from(value: u64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for WalletConnectJsonRpcId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for WalletConnectJsonRpcId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectJsonRpcResponse<R = Value, I = u64> {
    pub id: I,
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WalletConnectJsonRpcError>,
}

impl<R, I> WalletConnectJsonRpcResponse<R, I> {
    #[must_use]
    pub fn success(id: I, result: R) -> Self {
        Self {
            id,
            jsonrpc: "2.0".to_owned(),
            result: Some(result),
            error: None,
        }
    }

    pub fn into_result(self) -> Result<R> {
        if let Some(error) = self.error {
            return Err(WalletConnectError::Relay(format!(
                "{} ({})",
                error.message, error.code
            )));
        }
        self.result.ok_or_else(|| {
            WalletConnectError::Relay("relay response did not include a result".to_owned())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectRelaySubscriptionPayload {
    pub id: String,
    pub topic: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectRelaySubscriptionRequest {
    pub id: WalletConnectJsonRpcId,
    pub jsonrpc: String,
    pub method: String,
    pub params: WalletConnectRelaySubscriptionPayload,
}

impl WalletConnectRelaySubscriptionRequest {
    pub fn parse(value: &Value) -> Result<Option<Self>> {
        if value.get("method").and_then(Value::as_str) != Some("irn_subscription") {
            return Ok(None);
        }
        let id = value
            .get("id")
            .cloned()
            .ok_or_else(|| WalletConnectError::Relay("relay subscription missing id".to_owned()))
            .and_then(|value| {
                serde_json::from_value(value).map_err(|_| {
                    WalletConnectError::Relay(
                        "relay subscription id must be a string or number".to_owned(),
                    )
                })
            })?;
        let params = value.get("params").ok_or_else(|| {
            WalletConnectError::Relay("relay subscription missing params".to_owned())
        })?;
        let subscription_id = params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WalletConnectError::Relay("relay subscription missing subscription id".to_owned())
            })?
            .to_owned();
        let data = params.get("data").unwrap_or(params);
        let topic = data
            .get("topic")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WalletConnectError::Relay("relay subscription missing topic".to_owned())
            })?
            .to_owned();
        let message = data
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WalletConnectError::Relay("relay subscription missing message".to_owned())
            })?
            .to_owned();
        Ok(Some(Self {
            id,
            jsonrpc: "2.0".to_owned(),
            method: "irn_subscription".to_owned(),
            params: WalletConnectRelaySubscriptionPayload {
                id: subscription_id,
                topic,
                message,
            },
        }))
    }

    #[must_use]
    pub fn ack(&self) -> WalletConnectJsonRpcResponse<Value, WalletConnectJsonRpcId> {
        WalletConnectJsonRpcResponse::success(self.id.clone(), json!(true))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectJsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletConnectRelayRpc {
    Publish {
        topic: String,
        message: String,
        ttl: u64,
        tag: u32,
    },
    FetchMessages {
        topic: String,
    },
    BatchFetchMessages {
        topics: Vec<String>,
    },
    Subscribe {
        topic: String,
    },
    BatchSubscribe {
        topics: Vec<String>,
    },
    Unsubscribe {
        topic: String,
        id: String,
    },
}

impl WalletConnectRelayRpc {
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Publish { .. } => "irn_publish",
            Self::FetchMessages { .. } => "irn_fetchMessages",
            Self::BatchFetchMessages { .. } => "irn_batchFetchMessages",
            Self::Subscribe { .. } => "irn_subscribe",
            Self::BatchSubscribe { .. } => "irn_batchSubscribe",
            Self::Unsubscribe { .. } => "irn_unsubscribe",
        }
    }

    #[must_use]
    pub fn request(&self, id: u64) -> WalletConnectJsonRpcRequest<Value, String> {
        WalletConnectJsonRpcRequest {
            id: relay_request_id(id),
            jsonrpc: "2.0".to_owned(),
            method: self.method().to_owned(),
            params: self.params(),
        }
    }

    #[must_use]
    pub fn params(&self) -> Value {
        match self {
            Self::Publish {
                topic,
                message,
                ttl,
                tag,
            } => json!({
                "topic": topic,
                "message": message,
                "ttl": ttl,
                "tag": tag,
            }),
            Self::FetchMessages { topic } | Self::Subscribe { topic } => json!({ "topic": topic }),
            Self::BatchFetchMessages { topics } | Self::BatchSubscribe { topics } => {
                json!({ "topics": topics })
            }
            Self::Unsubscribe { topic, id } => json!({
                "topic": topic,
                "id": id,
            }),
        }
    }
}

#[derive(Clone)]
pub struct WalletConnectRelayClient {
    config: WalletConnectRelayConfig,
    auth: WalletConnectRelayClientAuth,
}

pub struct WalletConnectRelaySocket {
    websocket: reqwest_websocket::WebSocket,
    subscription_messages: VecDeque<WalletConnectRelaySubscriptionPayload>,
    pending_subscription_ack: Option<WalletConnectRelaySubscriptionRequest>,
}

impl WalletConnectRelayClient {
    #[must_use]
    pub const fn new(config: WalletConnectRelayConfig, auth: WalletConnectRelayClientAuth) -> Self {
        Self { config, auth }
    }

    #[must_use]
    pub const fn auth(&self) -> &WalletConnectRelayClientAuth {
        &self.auth
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.config.project_id
    }

    pub async fn send<R: DeserializeOwned>(
        &self,
        http: &HttpContext,
        id: u64,
        rpc: WalletConnectRelayRpc,
    ) -> Result<WalletConnectRelayJsonRpcResponse<R>> {
        let mut socket = self.connect(http).await?;
        socket.request(id, rpc).await
    }

    pub async fn connect(&self, http: &HttpContext) -> Result<WalletConnectRelaySocket> {
        tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            network_mode = %http.network_mode(),
            proxied = http.proxy_url.is_some(),
            project_id = %short_log_value(self.project_id()),
            "connecting walletconnect relay websocket"
        );
        let websocket_client = relay_websocket_client(http)?;
        let query = self.connection_query()?;
        let upgrade = websocket_client
            .get(WALLETCONNECT_RELAY_URL)
            .query(&query)
            .upgrade()
            .send()
            .await
            .map_err(relay_websocket_error)?;
        let websocket = upgrade
            .into_websocket()
            .await
            .map_err(relay_websocket_error)?;
        tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            network_mode = %http.network_mode(),
            client_id = %short_log_value(&self.auth.client_id),
            "connected walletconnect relay websocket"
        );
        Ok(WalletConnectRelaySocket {
            websocket,
            subscription_messages: VecDeque::new(),
            pending_subscription_ack: None,
        })
    }

    pub fn connection_query(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![
            (
                "auth".to_owned(),
                self.auth
                    .client_auth_jwt(WALLETCONNECT_RELAY_URL, WALLETCONNECT_RELAY_AUTH_TTL_SECS)?,
            ),
            ("projectId".to_owned(), self.project_id().to_owned()),
            ("ua".to_owned(), relay_user_agent()),
            ("useOnCloseEvent".to_owned(), "true".to_owned()),
        ])
    }
}

impl WalletConnectRelaySocket {
    pub async fn request<R: DeserializeOwned>(
        &mut self,
        id: u64,
        rpc: WalletConnectRelayRpc,
    ) -> Result<WalletConnectRelayJsonRpcResponse<R>> {
        self.buffer_pending_subscription_message().await?;
        let request = rpc.request(id);
        trace_relay_request_start(id, &rpc);
        self.websocket
            .send(Message::Text(serde_json::to_string(&request)?))
            .await
            .map_err(|error| relay_transport_error("send relay request", error))?;

        let deadline =
            Instant::now() + Duration::from_secs(WALLETCONNECT_RELAY_RESPONSE_TIMEOUT_SECS);
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(relay_timeout_error(id, rpc.method()));
            }
            let message = timeout(deadline - now, self.websocket.try_next())
                .await
                .map_err(|_| relay_timeout_error(id, rpc.method()))?
                .map_err(|error| relay_transport_error("read relay response", error))?
                .ok_or_else(|| relay_closed_error(id, rpc.method()))?;
            let Some(value) = websocket_message_json(message)? else {
                continue;
            };
            if let Some(subscription) = WalletConnectRelaySubscriptionRequest::parse(&value)? {
                trace_relay_subscription_message(&subscription.params);
                self.pending_subscription_ack = Some(subscription);
                self.buffer_pending_subscription_message().await?;
                continue;
            }
            if relay_response_id_matches(&value, id) {
                trace_relay_response(id, &rpc, &value);
                return Ok(serde_json::from_value(value)?);
            }
        }
    }

    pub fn drain_subscription_messages(&mut self) -> Vec<WalletConnectRelaySubscriptionPayload> {
        self.subscription_messages.drain(..).collect()
    }

    pub async fn collect_subscription_messages(
        &mut self,
        wait: Duration,
    ) -> Result<Vec<WalletConnectRelaySubscriptionPayload>> {
        self.buffer_pending_subscription_message().await?;
        let max_deadline = tokio::time::Instant::now() + wait;
        let mut idle_deadline = if self.subscription_messages.is_empty() {
            None
        } else {
            Some(
                tokio::time::Instant::now()
                    + Duration::from_millis(WALLETCONNECT_RELAY_SUBSCRIPTION_IDLE_WAIT_MILLIS),
            )
        };
        loop {
            let now = tokio::time::Instant::now();
            let deadline = idle_deadline.map_or(max_deadline, |idle_deadline| {
                if idle_deadline < max_deadline {
                    idle_deadline
                } else {
                    max_deadline
                }
            });
            if now >= deadline {
                return Ok(self.drain_subscription_messages());
            }
            let message = match timeout(deadline - now, self.websocket.try_next()).await {
                Ok(Ok(Some(message))) => message,
                Ok(Ok(None)) | Err(_) => return Ok(self.drain_subscription_messages()),
                Ok(Err(error)) => {
                    return Err(relay_transport_error("read relay subscription", error));
                }
            };
            let Some(value) = websocket_message_json(message)? else {
                continue;
            };
            if let Some(subscription) = WalletConnectRelaySubscriptionRequest::parse(&value)? {
                trace_relay_subscription_message(&subscription.params);
                self.pending_subscription_ack = Some(subscription);
                self.buffer_pending_subscription_message().await?;
                idle_deadline = Some(
                    tokio::time::Instant::now()
                        + Duration::from_millis(WALLETCONNECT_RELAY_SUBSCRIPTION_IDLE_WAIT_MILLIS),
                );
            }
        }
    }

    pub async fn next_subscription_message(
        &mut self,
    ) -> Result<Option<WalletConnectRelaySubscriptionPayload>> {
        loop {
            if let Some(payload) = self.acknowledge_pending_subscription().await? {
                return Ok(Some(payload));
            }
            let message = self
                .websocket
                .try_next()
                .await
                .map_err(|error| relay_transport_error("read relay subscription", error))?;
            let Some(message) = message else {
                return Ok(None);
            };
            let Some(value) = websocket_message_json(message)? else {
                continue;
            };
            if let Some(subscription) = WalletConnectRelaySubscriptionRequest::parse(&value)? {
                trace_relay_subscription_message(&subscription.params);
                self.pending_subscription_ack = Some(subscription);
                continue;
            }
            tracing::debug!(
                target: "wallet_ops::walletconnect::relay",
                "ignored walletconnect relay message without pending request"
            );
        }
    }

    async fn buffer_pending_subscription_message(&mut self) -> Result<()> {
        if let Some(payload) = self.acknowledge_pending_subscription().await? {
            self.subscription_messages.push_back(payload);
        }
        Ok(())
    }

    async fn acknowledge_pending_subscription(
        &mut self,
    ) -> Result<Option<WalletConnectRelaySubscriptionPayload>> {
        let Some(ack) = self
            .pending_subscription_ack
            .as_ref()
            .map(WalletConnectRelaySubscriptionRequest::ack)
        else {
            return Ok(None);
        };
        self.websocket
            .send(Message::Text(serde_json::to_string(&ack)?))
            .await
            .map_err(|error| relay_transport_error("ack relay subscription", error))?;
        let subscription = self
            .pending_subscription_ack
            .take()
            .expect("pending subscription ack exists");
        Ok(Some(subscription.params))
    }
}

fn relay_websocket_client(http: &HttpContext) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().http1_only().referer(false);
    if let Some(proxy_url) = http.proxy_url.as_ref() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy_url.as_str())
                .map_err(|error| relay_transport_error("configure relay proxy", error))?,
        );
    }
    if http.fail_closed() {
        // Offering `h2` would let the relay pick a protocol this HTTP/1.1 upgrade cannot use.
        let tls = crate::http::tls_config_without_resumption(&[b"http/1.1"])
            .map_err(|error| relay_transport_error("configure relay TLS", error))?;
        builder = builder
            .pool_max_idle_per_host(0)
            .tls_backend_preconfigured(tls);
    }
    builder
        .build()
        .map_err(|error| relay_transport_error("build relay websocket client", error))
}

#[allow(clippy::needless_pass_by_value)]
fn relay_websocket_error(error: reqwest_websocket::Error) -> WalletConnectError {
    let safe_error = sanitize_walletconnect_relay_error_message(&format!("{error:?}"));
    tracing::warn!(
        target: "wallet_ops::walletconnect::relay",
        error = %safe_error,
        "walletconnect relay websocket upgrade failed"
    );
    WalletConnectError::Relay(format!("websocket upgrade failed: {safe_error}"))
}

fn relay_transport_error(context: &'static str, error: impl fmt::Display) -> WalletConnectError {
    let safe_error = sanitize_walletconnect_relay_error_message(&error.to_string());
    tracing::warn!(
        target: "wallet_ops::walletconnect::relay",
        %context,
        error = %safe_error,
        "walletconnect relay transport failed"
    );
    WalletConnectError::Relay(format!("{context}: {safe_error}"))
}

fn relay_timeout_error(id: u64, method: &'static str) -> WalletConnectError {
    tracing::warn!(
        target: "wallet_ops::walletconnect::relay",
        request_id = id,
        method,
        "walletconnect relay response timed out"
    );
    WalletConnectError::Relay("relay response timed out".to_owned())
}

fn relay_closed_error(id: u64, method: &'static str) -> WalletConnectError {
    tracing::warn!(
        target: "wallet_ops::walletconnect::relay",
        request_id = id,
        method,
        "walletconnect relay websocket closed"
    );
    WalletConnectError::Relay("relay websocket closed".to_owned())
}

fn relay_user_agent() -> String {
    format!(
        "{WALLETCONNECT_RELAY_PROTOCOL}-{WALLETCONNECT_RELAY_PROTOCOL_VERSION}/rust-{WALLETCONNECT_RELAY_SDK_VERSION}/unknown/railoxide-wallet"
    )
}

fn websocket_message_json(message: Message) -> Result<Option<Value>> {
    match message {
        Message::Text(text) => Ok(Some(serde_json::from_str(&text)?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Message::Ping(_) | Message::Pong(_) | Message::Close { .. } => Ok(None),
    }
}

fn relay_request_id(id: u64) -> String {
    id.to_string()
}

pub(crate) fn relay_response_id_matches(value: &Value, id: u64) -> bool {
    let Some(response_id) = value.get("id") else {
        return false;
    };
    response_id.as_u64() == Some(id)
        || response_id
            .as_str()
            .is_some_and(|response_id| response_id == relay_request_id(id))
}

fn trace_relay_request_start(id: u64, rpc: &WalletConnectRelayRpc) {
    match rpc {
        WalletConnectRelayRpc::Publish {
            topic,
            message,
            ttl,
            tag,
        } => tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            request_id = id,
            method = rpc.method(),
            topic = %topic_log_label(topic),
            message_len = message.len(),
            ttl,
            tag,
            "sending walletconnect relay request"
        ),
        WalletConnectRelayRpc::FetchMessages { topic }
        | WalletConnectRelayRpc::Subscribe { topic } => tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            request_id = id,
            method = rpc.method(),
            topic = %topic_log_label(topic),
            "sending walletconnect relay request"
        ),
        WalletConnectRelayRpc::BatchFetchMessages { topics }
        | WalletConnectRelayRpc::BatchSubscribe { topics } => {
            let topics = topics
                .iter()
                .map(|topic| topic_log_label(topic))
                .collect::<Vec<_>>();
            tracing::debug!(
                target: "wallet_ops::walletconnect::relay",
                request_id = id,
                method = rpc.method(),
                topic_count = topics.len(),
                topics = ?topics,
                "sending walletconnect relay request"
            );
        }
        WalletConnectRelayRpc::Unsubscribe {
            topic,
            id: subscription_id,
        } => tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            request_id = id,
            method = rpc.method(),
            topic = %topic_log_label(topic),
            subscription_id = %short_log_value(subscription_id),
            "sending walletconnect relay request"
        ),
    }
}

fn trace_relay_response(id: u64, rpc: &WalletConnectRelayRpc, value: &Value) {
    if let Some(error) = value.get("error") {
        tracing::warn!(
            target: "wallet_ops::walletconnect::relay",
            request_id = id,
            method = rpc.method(),
            error = %sanitize_walletconnect_relay_error_message(&error.to_string()),
            "walletconnect relay request failed"
        );
        return;
    }
    match rpc {
        WalletConnectRelayRpc::FetchMessages { topic } => {
            let result = value.get("result").unwrap_or(value);
            tracing::debug!(
                target: "wallet_ops::walletconnect::relay",
                request_id = id,
                method = rpc.method(),
                topic = %topic_log_label(topic),
                message_count = relay_fetch_message_count(result).unwrap_or(0),
                has_more = relay_fetch_has_more(result).unwrap_or(false),
                "walletconnect relay request completed"
            );
        }
        WalletConnectRelayRpc::Subscribe { topic } => {
            let result = value.get("result").unwrap_or(value);
            tracing::debug!(
                target: "wallet_ops::walletconnect::relay",
                request_id = id,
                method = rpc.method(),
                topic = %topic_log_label(topic),
                subscription_id = %relay_subscription_id_log_label(result),
                "walletconnect relay request completed"
            );
        }
        _ => tracing::debug!(
            target: "wallet_ops::walletconnect::relay",
            request_id = id,
            method = rpc.method(),
            "walletconnect relay request completed"
        ),
    }
}

fn trace_relay_subscription_message(payload: &WalletConnectRelaySubscriptionPayload) {
    tracing::debug!(
        target: "wallet_ops::walletconnect::relay",
        subscription_id = %short_log_value(&payload.id),
        topic = %topic_log_label(&payload.topic),
        message_len = payload.message.len(),
        "received walletconnect relay subscription message"
    );
}

fn relay_fetch_message_count(value: &Value) -> Option<usize> {
    value
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .map(Vec::len)
}

fn relay_fetch_has_more(value: &Value) -> Option<bool> {
    value.get("hasMore").and_then(Value::as_bool)
}

fn relay_subscription_id_log_label(value: &Value) -> String {
    value
        .as_str()
        .or_else(|| value.get("subscriptionId").and_then(Value::as_str))
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map_or_else(|| "<missing>".to_owned(), short_log_value)
}

fn topic_log_label(topic: &str) -> String {
    short_log_value_with_prefix(topic, WALLETCONNECT_RELAY_TOPIC_LOG_PREFIX)
}

fn short_log_value(value: &str) -> String {
    const PREFIX: usize = 8;
    short_log_value_with_prefix(value, PREFIX)
}

fn short_log_value_with_prefix(value: &str, prefix_chars: usize) -> String {
    let prefix = value.chars().take(prefix_chars).collect::<String>();
    if prefix.len() == value.len() {
        prefix
    } else {
        format!("{}.../{}", prefix, value.len())
    }
}

pub(crate) fn sanitize_walletconnect_relay_error_message(message: &str) -> String {
    let without_urls = redact_url_queries(message);
    let without_query_values = redact_sensitive_query_values(&without_urls);
    let without_jwts = redact_jwt_like_tokens(&without_query_values);
    let trimmed = without_jwts.trim();
    if trimmed.is_empty() {
        "relay connection failed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn redact_url_queries(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut index = 0;
    while index < message.len() {
        let rest = &message[index..];
        if starts_with_url_scheme(rest) {
            let token_len = rest
                .char_indices()
                .find_map(|(offset, ch)| is_url_delimiter(ch).then_some(offset))
                .unwrap_or(rest.len());
            let token = &rest[..token_len];
            output.push_str(&sanitize_url_token(token));
            index += token_len;
            continue;
        }
        let ch = rest.chars().next().expect("string is not empty");
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

fn starts_with_url_scheme(value: &str) -> bool {
    value.starts_with("wss://")
        || value.starts_with("ws://")
        || value.starts_with("https://")
        || value.starts_with("http://")
}

const fn is_url_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | '<' | '>' | ')' | ']' | '}' | ',')
}

fn sanitize_url_token(token: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(token) {
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        return url.to_string();
    }
    token.split(['?', '#']).next().unwrap_or(token).to_owned()
}

fn redact_sensitive_query_values(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut index = 0;
    while index < message.len() {
        let rest = &message[index..];
        if let Some(key_len) = sensitive_query_key_len(rest) {
            output.push_str("<redacted-query-param>");
            index += key_len;
            while index < message.len() {
                let ch = message[index..]
                    .chars()
                    .next()
                    .expect("string is not empty");
                if is_query_value_delimiter(ch) {
                    break;
                }
                index += ch.len_utf8();
            }
            continue;
        }
        let ch = rest.chars().next().expect("string is not empty");
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

fn sensitive_query_key_len(value: &str) -> Option<usize> {
    ["auth=", "projectId=", "ua=", "useOnCloseEvent=", "symKey="]
        .iter()
        .find_map(|key| value.starts_with(key).then_some(key.len()))
}

const fn is_query_value_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | '`' | '<' | '>' | ')' | ']' | '}' | ',' | ';' | '&'
        )
}

fn redact_jwt_like_tokens(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut token = String::new();
    for ch in message.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            token.push(ch);
            continue;
        }
        push_redacted_token(&mut output, &token);
        token.clear();
        output.push(ch);
    }
    push_redacted_token(&mut output, &token);
    output
}

fn push_redacted_token(output: &mut String, token: &str) {
    if token.is_empty() {
        return;
    }
    if is_jwt_like_token(token) {
        output.push_str("<redacted-jwt>");
    } else {
        output.push_str(token);
    }
}

fn is_jwt_like_token(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    [header, payload, signature].iter().all(|part| {
        part.len() >= 8
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    })
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_log_labels_accept_non_ascii_values() {
        let subscription_id = "ééééééééé";
        let topic = "主题主题主题主题主题主题";

        let subscription_label = short_log_value(subscription_id);
        let topic_label = topic_log_label(topic);

        assert!(subscription_label.starts_with("éééééééé"));
        assert!(subscription_label.ends_with(&format!("/{}", subscription_id.len())));
        assert!(topic_label.starts_with("主题主题主题主题主题"));
        assert!(topic_label.ends_with(&format!("/{}", topic.len())));
    }
}
