use std::collections::BTreeMap;
use std::str::FromStr;

use alloy::dyn_abi::TypedData;
use alloy::hex;
use alloy::primitives::{Address, U256};
use alloy::rpc::types::transaction::AccessList;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{Value, json};

use crate::vault::{
    WalletConnectSessionAccountResolution, WalletConnectSessionLifecycleState,
    WalletConnectSessionRecord,
};

use super::eip155::WalletConnectSupportedMethod;
use super::namespace::{
    WalletConnectNamespaceAccountSupport, walletconnect_method_supported_for_account_support,
};
use super::relay::{
    WalletConnectJsonRpcError, WalletConnectJsonRpcRequest, WalletConnectJsonRpcResponse,
};
use super::{Result, WalletConnectError};

const WC_SESSION_REQUEST_EXPIRED: i64 = 8_000;
const WC_SESSION_REQUEST_MAX_EXPIRY_INTERVAL_SECS: u64 = 604_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletConnectRequestErrorKind {
    UserRejected,
    UnsupportedMethod,
    UnsupportedChain,
    MalformedParams,
    ExpiredRequest,
    Unauthorized,
    Internal,
}

impl WalletConnectRequestErrorKind {
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::UserRejected => 4_001,
            Self::UnsupportedMethod => 4_200,
            Self::UnsupportedChain => 4_902,
            Self::MalformedParams => -32_602,
            Self::ExpiredRequest => WC_SESSION_REQUEST_EXPIRED,
            Self::Unauthorized => 4_100,
            Self::Internal => -32_603,
        }
    }
}

pub fn build_walletconnect_jsonrpc_error(
    id: u64,
    kind: WalletConnectRequestErrorKind,
    message: impl Into<String>,
) -> WalletConnectJsonRpcResponse<Value> {
    WalletConnectJsonRpcResponse {
        id,
        jsonrpc: "2.0".to_owned(),
        result: None,
        error: Some(WalletConnectJsonRpcError {
            code: kind.code(),
            message: message.into(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum WalletConnectParsedRequest {
    EthAccounts,
    EthRequestAccounts,
    PersonalSign {
        message: String,
        account: Address,
    },
    EthSendTransaction {
        transaction: WalletConnectEvmTransaction,
    },
    EthSignTypedData {
        account: Address,
        typed_data: Value,
        domain_chain_id: Option<U256>,
    },
    EthSignTypedDataV4 {
        account: Address,
        typed_data: Value,
        domain_chain_id: Option<U256>,
    },
    WalletSwitchEthereumChain {
        chain_id: u64,
    },
}

impl WalletConnectParsedRequest {
    #[must_use]
    pub const fn method(&self) -> WalletConnectSupportedMethod {
        match self {
            Self::EthAccounts => WalletConnectSupportedMethod::EthAccounts,
            Self::EthRequestAccounts => WalletConnectSupportedMethod::EthRequestAccounts,
            Self::PersonalSign { .. } => WalletConnectSupportedMethod::PersonalSign,
            Self::EthSendTransaction { .. } => WalletConnectSupportedMethod::EthSendTransaction,
            Self::EthSignTypedData { .. } => WalletConnectSupportedMethod::EthSignTypedData,
            Self::EthSignTypedDataV4 { .. } => WalletConnectSupportedMethod::EthSignTypedDataV4,
            Self::WalletSwitchEthereumChain { .. } => {
                WalletConnectSupportedMethod::WalletSwitchEthereumChain
            }
        }
    }

    #[must_use]
    pub const fn approval_required(&self) -> bool {
        matches!(
            self,
            Self::PersonalSign { .. }
                | Self::EthSendTransaction { .. }
                | Self::EthSignTypedData { .. }
                | Self::EthSignTypedDataV4 { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectEvmTransaction {
    pub from: Address,
    pub to: Option<Address>,
    pub value: Option<U256>,
    pub data: Option<String>,
    pub access_list: Option<AccessList>,
    pub gas: Option<U256>,
    pub gas_price: Option<U256>,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee_per_gas: Option<U256>,
    pub chain_id: Option<u64>,
    pub nonce: Option<U256>,
    pub transaction_type: Option<u8>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectRequestValidation {
    pub request: WalletConnectParsedRequest,
    pub chain_id: String,
    pub account: Option<Address>,
    pub approval_item: Option<WalletConnectPendingRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectPendingRequest {
    pub id: u64,
    pub topic: String,
    pub dapp_name: String,
    pub chain_id: String,
    pub method: WalletConnectSupportedMethod,
    pub account: Address,
    pub decoded_transaction: Option<WalletConnectDecodedTransaction>,
    pub raw_details: Value,
    pub expiry_timestamp: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectDecodedTransaction {
    pub target: Option<Address>,
    pub native_value: U256,
    pub kind: WalletConnectDecodedCallKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletConnectDecodedCallKind {
    NativeTransfer,
    Erc20Approve {
        spender: Address,
        amount: U256,
    },
    Erc20Transfer {
        recipient: Address,
        amount: U256,
    },
    Erc20TransferFrom {
        from: Address,
        to: Address,
        amount: U256,
    },
    WrappedDeposit,
    WrappedWithdraw {
        amount: U256,
    },
    ContractCall {
        selector: Option<[u8; 4]>,
    },
    ContractCreation,
}

sol! {
    interface Erc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address recipient, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
    }

    interface WrappedNative {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
    }
}

impl WalletConnectDecodedCallKind {
    #[must_use]
    pub const fn selector(&self) -> Option<[u8; 4]> {
        match self {
            Self::Erc20Approve { .. } => Some(Erc20::approveCall::SELECTOR),
            Self::Erc20Transfer { .. } => Some(Erc20::transferCall::SELECTOR),
            Self::Erc20TransferFrom { .. } => Some(Erc20::transferFromCall::SELECTOR),
            Self::WrappedDeposit => Some(WrappedNative::depositCall::SELECTOR),
            Self::WrappedWithdraw { .. } => Some(WrappedNative::withdrawCall::SELECTOR),
            Self::ContractCall { selector } => *selector,
            Self::NativeTransfer | Self::ContractCreation => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalletConnectPendingRequestQueue {
    pending: BTreeMap<u64, WalletConnectPendingRequest>,
}

impl WalletConnectPendingRequestQueue {
    pub fn insert(&mut self, request: WalletConnectPendingRequest) {
        self.pending.insert(request.id, request);
    }

    pub fn remove(&mut self, id: u64) -> Option<WalletConnectPendingRequest> {
        self.pending.remove(&id)
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<&WalletConnectPendingRequest> {
        self.pending.get(&id)
    }

    pub fn remove_expired(&mut self, now_unix_seconds: u64) -> Vec<WalletConnectPendingRequest> {
        let expired = self
            .pending
            .iter()
            .filter_map(|(id, request)| {
                request
                    .expiry_timestamp
                    .is_some_and(|expiry| expiry <= now_unix_seconds)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|id| self.pending.remove(&id))
            .collect()
    }
}

pub fn parse_walletconnect_session_request(
    id: u64,
    method: &str,
    params: &Value,
) -> Result<WalletConnectParsedRequest> {
    match method {
        "eth_accounts" => Ok(WalletConnectParsedRequest::EthAccounts),
        "eth_requestAccounts" => Ok(WalletConnectParsedRequest::EthRequestAccounts),
        "personal_sign" => parse_personal_sign(params),
        "eth_sendTransaction" => parse_send_transaction(params),
        "eth_signTypedData" => {
            parse_typed_data(WalletConnectSupportedMethod::EthSignTypedData, params)
        }
        "eth_signTypedData_v4" => {
            parse_typed_data(WalletConnectSupportedMethod::EthSignTypedDataV4, params)
        }
        "wallet_switchEthereumChain" => parse_switch_chain(params),
        "eth_sign"
        | "eth_signTypedData_v3"
        | "eth_signTransaction"
        | "eth_sendRawTransaction"
        | "wallet_addEthereumChain" => {
            Err(WalletConnectError::UnsupportedMethod(method.to_owned()))
        }
        other => Err(WalletConnectError::UnsupportedMethod(format!(
            "{id}:{other}"
        ))),
    }
}

pub fn validate_walletconnect_session_request(
    session: &WalletConnectSessionRecord,
    account_resolution: &WalletConnectSessionAccountResolution,
    topic: &str,
    id: u64,
    chain_id: &str,
    request: WalletConnectParsedRequest,
    expiry_timestamp: Option<u64>,
    now_unix_seconds: u64,
) -> Result<WalletConnectRequestValidation> {
    let selected_account_support = match account_resolution {
        WalletConnectSessionAccountResolution::Usable(account) => {
            WalletConnectNamespaceAccountSupport::for_account_source(account.source)
        }
        WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { .. }
        | WalletConnectSessionAccountResolution::InvalidPublicAccount => {
            WalletConnectNamespaceAccountSupport::for_account_source(
                crate::vault::PublicAccountSource::Derived,
            )
        }
    };
    validate_walletconnect_session_request_with_account_support(
        session,
        account_resolution,
        selected_account_support,
        topic,
        id,
        chain_id,
        request,
        expiry_timestamp,
        now_unix_seconds,
    )
}

pub fn validate_walletconnect_session_request_with_account_support(
    session: &WalletConnectSessionRecord,
    account_resolution: &WalletConnectSessionAccountResolution,
    selected_account_support: WalletConnectNamespaceAccountSupport,
    topic: &str,
    id: u64,
    chain_id: &str,
    request: WalletConnectParsedRequest,
    expiry_timestamp: Option<u64>,
    now_unix_seconds: u64,
) -> Result<WalletConnectRequestValidation> {
    if topic != session.session_topic {
        return Err(WalletConnectError::Relay(
            "request topic does not match session".to_owned(),
        ));
    }
    if session.lifecycle_state != WalletConnectSessionLifecycleState::Active {
        return Err(WalletConnectError::Relay(
            "session is not active".to_owned(),
        ));
    }
    if session.expiry_timestamp <= now_unix_seconds {
        return Err(WalletConnectError::ExpiredUri);
    }
    if let Some(expiry_timestamp) = expiry_timestamp {
        validate_session_request_expiry_timestamp(expiry_timestamp, now_unix_seconds)?;
    }

    let selected_account = match account_resolution {
        WalletConnectSessionAccountResolution::Usable(account) => account,
        WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { .. } => {
            return Err(WalletConnectError::Relay(
                "session is paused for a different selected Private wallet".to_owned(),
            ));
        }
        WalletConnectSessionAccountResolution::InvalidPublicAccount => {
            return Err(WalletConnectError::Relay(
                "session Public account is invalid".to_owned(),
            ));
        }
    };

    let request_chain_id = parse_caip2_eip155_chain(chain_id)
        .ok_or_else(|| WalletConnectError::UnsupportedChain(chain_id.to_owned()))?;
    ensure_method_approved(session, chain_id, request.method())?;
    if !walletconnect_approved_request_method_supported_for_account_support(
        request.method(),
        selected_account_support,
    ) {
        return Err(WalletConnectError::UnsupportedMethod(
            request.method().as_str().to_owned(),
        ));
    }

    let account = request_account(&request);
    if let Some(account) = account
        && account != selected_account.address
    {
        return Err(WalletConnectError::Relay(
            "request account does not match selected Public account".to_owned(),
        ));
    }

    match &request {
        WalletConnectParsedRequest::EthSendTransaction { transaction } => {
            if transaction.from != selected_account.address {
                return Err(WalletConnectError::Relay(
                    "transaction from does not match selected Public account".to_owned(),
                ));
            }
            if transaction
                .chain_id
                .is_some_and(|embedded| embedded != request_chain_id)
            {
                return Err(WalletConnectError::Relay(
                    "transaction chainId does not match request chain".to_owned(),
                ));
            }
        }
        WalletConnectParsedRequest::EthSignTypedData {
            domain_chain_id, ..
        }
        | WalletConnectParsedRequest::EthSignTypedDataV4 {
            domain_chain_id, ..
        } => {
            if domain_chain_id.is_some_and(|embedded| embedded != U256::from(request_chain_id)) {
                return Err(WalletConnectError::Relay(
                    "typed-data domain.chainId does not match request chain".to_owned(),
                ));
            }
        }
        WalletConnectParsedRequest::WalletSwitchEthereumChain {
            chain_id: switch_chain,
        } => {
            ensure_chain_approved(session, &format!("eip155:{switch_chain}"))?;
        }
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts
        | WalletConnectParsedRequest::PersonalSign { .. } => {}
    }

    let approval_item = if request.approval_required() {
        let account = account.unwrap_or(selected_account.address);
        Some(WalletConnectPendingRequest {
            id,
            topic: topic.to_owned(),
            dapp_name: session.peer_metadata.name.clone(),
            chain_id: chain_id.to_owned(),
            method: request.method(),
            account,
            decoded_transaction: match &request {
                WalletConnectParsedRequest::EthSendTransaction { transaction } => {
                    Some(decode_walletconnect_transaction(transaction))
                }
                _ => None,
            },
            raw_details: request_raw_details(&request),
            expiry_timestamp,
        })
    } else {
        None
    };

    Ok(WalletConnectRequestValidation {
        request,
        chain_id: chain_id.to_owned(),
        account,
        approval_item,
    })
}

const fn walletconnect_approved_request_method_supported_for_account_support(
    method: WalletConnectSupportedMethod,
    selected_account_support: WalletConnectNamespaceAccountSupport,
) -> bool {
    if walletconnect_method_supported_for_account_support(method, selected_account_support) {
        return true;
    }
    matches!(
        (
            method,
            selected_account_support.account_source,
            selected_account_support.hardware_typed_data_capability_known,
        ),
        (
            WalletConnectSupportedMethod::EthSignTypedData
                | WalletConnectSupportedMethod::EthSignTypedDataV4,
            crate::vault::PublicAccountSource::HardwareDerived,
            false,
        )
    )
}

#[allow(clippy::needless_pass_by_value)]
pub fn build_walletconnect_session_event(
    session: &WalletConnectSessionRecord,
    id: u64,
    chain_id: &str,
    event_name: &str,
    data: Value,
) -> Result<WalletConnectJsonRpcRequest<Value>> {
    let mut chain_approved = false;
    for namespace in namespaces_for_chain(session, chain_id) {
        chain_approved = true;
        if namespace.events.iter().any(|event| event == event_name) {
            return Ok(WalletConnectJsonRpcRequest::new(
                id,
                "wc_sessionEvent",
                json!({
                    "chainId": chain_id,
                    "event": {
                        "name": event_name,
                        "data": data,
                    },
                }),
            ));
        }
    }
    if chain_approved {
        Err(WalletConnectError::UnsupportedEvent(event_name.to_owned()))
    } else {
        Err(WalletConnectError::UnsupportedChain(chain_id.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletConnectLifecycleRequestOutcome {
    Delete {
        response: WalletConnectJsonRpcResponse<Value>,
    },
    Ping {
        response: WalletConnectJsonRpcResponse<Value>,
    },
    NotLifecycleRequest,
}

#[must_use]
pub fn handle_walletconnect_lifecycle_request(
    id: u64,
    method: &str,
) -> WalletConnectLifecycleRequestOutcome {
    match method {
        "wc_sessionDelete" => WalletConnectLifecycleRequestOutcome::Delete {
            response: WalletConnectJsonRpcResponse {
                id,
                jsonrpc: "2.0".to_owned(),
                result: Some(json!(true)),
                error: None,
            },
        },
        "wc_sessionPing" => WalletConnectLifecycleRequestOutcome::Ping {
            response: WalletConnectJsonRpcResponse {
                id,
                jsonrpc: "2.0".to_owned(),
                result: Some(json!(true)),
                error: None,
            },
        },
        _ => WalletConnectLifecycleRequestOutcome::NotLifecycleRequest,
    }
}

fn parse_personal_sign(params: &Value) -> Result<WalletConnectParsedRequest> {
    let values = params
        .as_array()
        .ok_or_else(|| malformed_params("personal_sign params must be an array"))?;
    let message = values
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| malformed_params("personal_sign message is required"))?;
    if let Some(encoded) = message.strip_prefix("0x")
        && (!encoded.len().is_multiple_of(2) || hex::decode(encoded).is_err())
    {
        return Err(malformed_params(
            "personal_sign message must be valid hex when prefixed with 0x",
        ));
    }
    let account = parse_address_value(values.get(1))
        .ok_or_else(|| malformed_params("personal_sign account is required"))?;
    Ok(WalletConnectParsedRequest::PersonalSign {
        message: message.to_owned(),
        account,
    })
}

fn parse_send_transaction(params: &Value) -> Result<WalletConnectParsedRequest> {
    let tx = params
        .as_array()
        .and_then(|values| values.first())
        .ok_or_else(|| malformed_params("transaction params are required"))?;
    let from = parse_address_field(tx, "from")?;
    let transaction = WalletConnectEvmTransaction {
        from,
        to: parse_optional_address_field(tx, "to")?,
        value: parse_optional_u256_field(tx, "value")?,
        data: parse_optional_data_field(tx)?,
        access_list: parse_optional_access_list_field(tx)?,
        gas: parse_optional_u256_field(tx, "gas")?,
        gas_price: parse_optional_u256_field(tx, "gasPrice")?,
        max_fee_per_gas: parse_optional_u256_field(tx, "maxFeePerGas")?,
        max_priority_fee_per_gas: parse_optional_u256_field(tx, "maxPriorityFeePerGas")?,
        chain_id: parse_optional_u64_field(tx, "chainId")?,
        nonce: parse_optional_u256_field(tx, "nonce")?,
        transaction_type: parse_optional_transaction_type(tx)?,
        raw: tx.clone(),
    };
    Ok(WalletConnectParsedRequest::EthSendTransaction { transaction })
}

fn parse_typed_data(
    method: WalletConnectSupportedMethod,
    params: &Value,
) -> Result<WalletConnectParsedRequest> {
    let method_name = method.as_str();
    let values = params
        .as_array()
        .ok_or_else(|| malformed_params(format!("{method_name} params must be an array")))?;
    let account = parse_address_value(values.first())
        .ok_or_else(|| malformed_params(format!("{method_name} account is required")))?;
    let typed_data = values
        .get(1)
        .cloned()
        .ok_or_else(|| malformed_params(format!("{method_name} payload is required")))?;
    let typed_data = if let Some(encoded) = typed_data.as_str() {
        serde_json::from_str(encoded).map_err(|error| {
            malformed_params(format!("{method_name} payload must be JSON: {error}"))
        })?
    } else {
        typed_data
    };
    let domain_chain_id = typed_data
        .get("domain")
        .and_then(|domain| domain.get("chainId"))
        .map(|value| {
            parse_u256_value(value).ok_or_else(|| {
                malformed_params(format!("{method_name} domain.chainId must be a chain ID"))
            })
        })
        .transpose()?;
    validate_typed_data_payload(method_name, &typed_data)?;
    let request = match method {
        WalletConnectSupportedMethod::EthSignTypedData => {
            WalletConnectParsedRequest::EthSignTypedData {
                account,
                typed_data,
                domain_chain_id,
            }
        }
        WalletConnectSupportedMethod::EthSignTypedDataV4 => {
            WalletConnectParsedRequest::EthSignTypedDataV4 {
                account,
                typed_data,
                domain_chain_id,
            }
        }
        _ => {
            return Err(WalletConnectError::UnsupportedMethod(
                method_name.to_owned(),
            ));
        }
    };
    Ok(request)
}

fn parse_switch_chain(params: &Value) -> Result<WalletConnectParsedRequest> {
    let chain_id = params
        .as_array()
        .and_then(|values| values.first())
        .and_then(|value| value.get("chainId"))
        .and_then(parse_u64_value)
        .ok_or_else(|| malformed_params("switch chainId is required"))?;
    Ok(WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id })
}

const fn request_account(request: &WalletConnectParsedRequest) -> Option<Address> {
    match request {
        WalletConnectParsedRequest::PersonalSign { account, .. }
        | WalletConnectParsedRequest::EthSignTypedData { account, .. }
        | WalletConnectParsedRequest::EthSignTypedDataV4 { account, .. } => Some(*account),
        WalletConnectParsedRequest::EthSendTransaction { transaction } => Some(transaction.from),
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts
        | WalletConnectParsedRequest::WalletSwitchEthereumChain { .. } => None,
    }
}

fn request_raw_details(request: &WalletConnectParsedRequest) -> Value {
    match request {
        WalletConnectParsedRequest::PersonalSign { message, account } => json!({
            "message": message,
            "account": account.to_string(),
        }),
        WalletConnectParsedRequest::EthSendTransaction { transaction } => transaction.raw.clone(),
        WalletConnectParsedRequest::EthSignTypedData { typed_data, .. }
        | WalletConnectParsedRequest::EthSignTypedDataV4 { typed_data, .. } => typed_data.clone(),
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts
        | WalletConnectParsedRequest::WalletSwitchEthereumChain { .. } => Value::Null,
    }
}

fn ensure_method_approved(
    session: &WalletConnectSessionRecord,
    chain_id: &str,
    method: WalletConnectSupportedMethod,
) -> Result<()> {
    let mut chain_approved = false;
    for namespace in namespaces_for_chain(session, chain_id) {
        chain_approved = true;
        if namespace
            .methods
            .iter()
            .any(|approved| approved == method.as_str())
        {
            return Ok(());
        }
    }
    if chain_approved {
        Err(WalletConnectError::UnsupportedMethod(
            method.as_str().to_owned(),
        ))
    } else {
        Err(WalletConnectError::UnsupportedChain(chain_id.to_owned()))
    }
}

fn ensure_chain_approved(session: &WalletConnectSessionRecord, chain_id: &str) -> Result<()> {
    if namespaces_for_chain(session, chain_id).next().is_some() {
        Ok(())
    } else {
        Err(WalletConnectError::UnsupportedChain(chain_id.to_owned()))
    }
}

fn namespaces_for_chain<'a>(
    session: &'a WalletConnectSessionRecord,
    chain_id: &str,
) -> impl Iterator<Item = &'a crate::vault::WalletConnectApprovedNamespace> {
    session
        .approved_namespaces
        .values()
        .filter(move |namespace| namespace.chains.iter().any(|chain| chain == chain_id))
}

fn parse_caip2_eip155_chain(value: &str) -> Option<u64> {
    value.strip_prefix("eip155:")?.parse().ok()
}

fn parse_address_field(value: &Value, field: &str) -> Result<Address> {
    value
        .get(field)
        .and_then(|value| parse_address_value(Some(value)))
        .ok_or_else(|| malformed_params(format!("{field} address is required")))
}

fn parse_optional_address_field(value: &Value, field: &str) -> Result<Option<Address>> {
    value
        .get(field)
        .map(|value| {
            parse_address_value(Some(value))
                .ok_or_else(|| malformed_params(format!("{field} must be an EVM address")))
        })
        .transpose()
}

fn parse_address_value(value: Option<&Value>) -> Option<Address> {
    Address::from_str(value?.as_str()?).ok()
}

fn parse_optional_u256_field(value: &Value, field: &str) -> Result<Option<U256>> {
    value
        .get(field)
        .map(|value| {
            parse_u256_value(value)
                .ok_or_else(|| malformed_params(format!("{field} must be a quantity")))
        })
        .transpose()
}

fn parse_optional_u64_field(value: &Value, field: &str) -> Result<Option<u64>> {
    value
        .get(field)
        .map(|value| {
            parse_u64_value(value)
                .ok_or_else(|| malformed_params(format!("{field} must be a chain ID")))
        })
        .transpose()
}

fn parse_optional_data_field(value: &Value) -> Result<Option<String>> {
    let Some(data) = value.get("data").or_else(|| value.get("input")) else {
        return Ok(None);
    };
    let text = data
        .as_str()
        .ok_or_else(|| malformed_params("transaction data must be a string"))?;
    let hex = text.strip_prefix("0x").unwrap_or(text);
    if !hex.len().is_multiple_of(2) || hex::decode(hex).is_err() {
        return Err(malformed_params("transaction data must be valid hex"));
    }
    Ok(Some(text.to_owned()))
}

fn parse_optional_access_list_field(value: &Value) -> Result<Option<AccessList>> {
    value
        .get("accessList")
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|error| {
                malformed_params(format!("transaction accessList is malformed: {error}"))
            })
        })
        .transpose()
}

fn parse_optional_transaction_type(value: &Value) -> Result<Option<u8>> {
    value
        .get("type")
        .map(|value| {
            let value = parse_u64_value(value)
                .ok_or_else(|| malformed_params("transaction type must be a quantity"))?;
            u8::try_from(value).map_err(|_| malformed_params("transaction type exceeds u8"))
        })
        .transpose()
}

fn validate_typed_data_payload(method: &str, value: &Value) -> Result<()> {
    let typed_data: TypedData = serde_json::from_value(value.clone()).map_err(|error| {
        malformed_params(format!("{method} payload is invalid EIP-712: {error}"))
    })?;
    typed_data.coerce().map_err(|error| {
        malformed_params(format!("{method} payload is invalid EIP-712: {error}"))
    })?;
    Ok(())
}

fn malformed_params(message: impl Into<String>) -> WalletConnectError {
    WalletConnectError::MalformedParams(message.into())
}

const fn validate_session_request_expiry_timestamp(expiry_timestamp: u64, now: u64) -> Result<()> {
    let max_expiry = now.saturating_add(WC_SESSION_REQUEST_MAX_EXPIRY_INTERVAL_SECS);
    if expiry_timestamp <= now || expiry_timestamp > max_expiry {
        return Err(WalletConnectError::ExpiredUri);
    }
    Ok(())
}

fn parse_u256_value(value: &Value) -> Option<U256> {
    if let Some(value) = value.as_u64() {
        return Some(U256::from(value));
    }
    let text = value.as_str()?;
    if let Some(hex) = text.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).ok()
    } else {
        U256::from_str_radix(text, 10).ok()
    }
}

fn parse_u64_value(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let text = value.as_str()?;
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

fn decode_walletconnect_transaction(
    transaction: &WalletConnectEvmTransaction,
) -> WalletConnectDecodedTransaction {
    let native_value = transaction.value.unwrap_or(U256::ZERO);
    let target = transaction.to;
    let encoded_data = transaction
        .data
        .as_deref()
        .map_or("", |data| data.strip_prefix("0x").unwrap_or(data));
    let decoded_data = hex::decode(encoded_data);
    let selector = decoded_data
        .as_ref()
        .ok()
        .and_then(|data| data.get(..4))
        .and_then(|selector| <[u8; 4]>::try_from(selector).ok())
        .or_else(|| decode_selector_prefix(encoded_data));

    let kind = match target {
        None => WalletConnectDecodedCallKind::ContractCreation,
        Some(_) => match decoded_data {
            Ok(data) if data.is_empty() => {
                if native_value.is_zero() {
                    WalletConnectDecodedCallKind::ContractCall { selector: None }
                } else {
                    WalletConnectDecodedCallKind::NativeTransfer
                }
            }
            Ok(data) => decode_call_kind(&data, selector),
            Err(_) => WalletConnectDecodedCallKind::ContractCall { selector },
        },
    };

    WalletConnectDecodedTransaction {
        target,
        native_value,
        kind,
    }
}

fn decode_call_kind(data: &[u8], selector: Option<[u8; 4]>) -> WalletConnectDecodedCallKind {
    let Some(selector) = selector else {
        return WalletConnectDecodedCallKind::ContractCall { selector: None };
    };
    let payload = &data[4..];
    match selector {
        Erc20::approveCall::SELECTOR if payload.len() == 64 => {
            match (decode_abi_address(payload, 0), decode_abi_u256(payload, 1)) {
                (Some(spender), Some(amount)) => {
                    WalletConnectDecodedCallKind::Erc20Approve { spender, amount }
                }
                _ => WalletConnectDecodedCallKind::ContractCall {
                    selector: Some(selector),
                },
            }
        }
        Erc20::transferCall::SELECTOR if payload.len() == 64 => {
            match (decode_abi_address(payload, 0), decode_abi_u256(payload, 1)) {
                (Some(recipient), Some(amount)) => {
                    WalletConnectDecodedCallKind::Erc20Transfer { recipient, amount }
                }
                _ => WalletConnectDecodedCallKind::ContractCall {
                    selector: Some(selector),
                },
            }
        }
        Erc20::transferFromCall::SELECTOR if payload.len() == 96 => match (
            decode_abi_address(payload, 0),
            decode_abi_address(payload, 1),
            decode_abi_u256(payload, 2),
        ) {
            (Some(from), Some(to), Some(amount)) => {
                WalletConnectDecodedCallKind::Erc20TransferFrom { from, to, amount }
            }
            _ => WalletConnectDecodedCallKind::ContractCall {
                selector: Some(selector),
            },
        },
        WrappedNative::depositCall::SELECTOR if data.len() == 4 => {
            WalletConnectDecodedCallKind::WrappedDeposit
        }
        WrappedNative::withdrawCall::SELECTOR if payload.len() == 32 => {
            WalletConnectDecodedCallKind::WrappedWithdraw {
                amount: U256::from_be_slice(payload),
            }
        }
        _ => WalletConnectDecodedCallKind::ContractCall {
            selector: Some(selector),
        },
    }
}

fn decode_selector_prefix(encoded_data: &str) -> Option<[u8; 4]> {
    let selector = encoded_data.get(..8)?;
    <[u8; 4]>::try_from(hex::decode(selector).ok()?).ok()
}

fn decode_abi_address(payload: &[u8], slot: usize) -> Option<Address> {
    let value = payload.get(slot * 32..slot * 32 + 32)?;
    if value[..12].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(Address::from_slice(value.get(12..32)?))
}

fn decode_abi_u256(payload: &[u8], slot: usize) -> Option<U256> {
    let value = payload.get(slot * 32..slot * 32 + 32)?;
    Some(U256::from_be_slice(value))
}
