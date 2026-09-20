use super::operation::{self, ReceiptBlock, RpcOperation};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rlp::Encodable;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::eth::state::StateOverride;
use alloy::rpc::types::eth::transaction::{TransactionInput, TransactionRequest};
use alloy::serde::WithOtherFields;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use poi::SensitiveUrl;
use serde_json::{Number, Value};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use url::Url;

pub(super) const DEFAULT_INTERVAL: Duration = Duration::from_millis(15);
pub(super) const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DEFAULT_MAX_CALLS: usize = 64;
pub(super) const DEFAULT_MAX_ESTIMATED_GAS: u64 = 8_000_000;
pub(super) const DEFAULT_READ_GAS: u64 = 100_000;
pub(super) const DEFAULT_MAX_IN_FLIGHT: usize = 4;
pub(super) const HEALTH_WINDOW: Duration = Duration::from_secs(30);
pub(super) const HEALTH_STRIKE_THRESHOLD: usize = 3;
pub(super) const HEALTH_WITHDRAWAL_BASE: Duration = Duration::from_secs(5);
pub(super) const HEALTH_WITHDRAWAL_MAX: Duration = Duration::from_hours(1);
pub(super) const MAX_REDUCTION_ATTEMPTS_PER_ENDPOINT: usize = 16;
pub(super) const HEAD_FRESHNESS_LEASE: Duration = Duration::from_mins(2);

const MAX_SUBMISSION_READS: usize = 64;
const MAX_READ_INPUT_BYTES: usize = 128 * 1024;
const MAX_SUBMISSION_INPUT_BYTES: usize = 512 * 1024;

sol! {
    interface RpcBrokerViewCalls {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
        function totalSupply() external view returns (uint256);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function quoteExactInputSingle(address tokenIn, address tokenOut, uint24 fee, uint256 amountIn, uint160 limitSqrtP)
            external view returns (uint256 amountOut);
        function latestAnswer() external view returns (int256);
    }
}

pub(super) const MSG_SENDER_INDEPENDENT: &[[u8; 4]] = &[
    RpcBrokerViewCalls::balanceOfCall::SELECTOR,
    RpcBrokerViewCalls::allowanceCall::SELECTOR,
    RpcBrokerViewCalls::decimalsCall::SELECTOR,
    RpcBrokerViewCalls::symbolCall::SELECTOR,
    RpcBrokerViewCalls::totalSupplyCall::SELECTOR,
    RpcBrokerViewCalls::getReservesCall::SELECTOR,
    RpcBrokerViewCalls::quoteExactInputSingleCall::SELECTOR,
    RpcBrokerViewCalls::latestAnswerCall::SELECTOR,
];

/// A validated read whose method and parameters are preserved during broker dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcRead {
    operation: RpcOperation,
}

/// Balance candidates extracted only from a fully validated read.
#[derive(Clone, Copy)]
pub(crate) enum BalanceRead {
    Native { account: Address },
    Token { account: Address, token: Address },
}
impl BalanceRead {
    pub(crate) const fn account(self) -> Address {
        match self {
            Self::Native { account } | Self::Token { account, .. } => account,
        }
    }

    pub(crate) fn encode_result(self, amount: U256) -> Value {
        match self {
            Self::Native { .. } => serde_json::json!(amount),
            Self::Token { .. } => serde_json::json!(Bytes::from(amount.abi_encode())),
        }
    }
}

/// Hash lookup classification for authorized local consumers of validated reads.
#[derive(Clone, Copy)]
pub(crate) enum TransactionHashRead {
    Transaction,
    Receipt,
}

/// Validation failures for RPC read parameters before they enter the broker.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RpcReadValidationError {
    #[error("invalid RPC read parameters")]
    InvalidParams,
    #[error("RPC read chainId does not match the submission route")]
    ChainIdMismatch,
}

impl RpcReadValidationError {
    /// JSON-RPC error code used by the gateway adapter for this rejection.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::InvalidParams => -32602,
            Self::ChainIdMismatch => -32000,
        }
    }
}

impl RpcRead {
    pub(crate) fn balance_candidate(&self) -> Option<BalanceRead> {
        match &self.operation {
            RpcOperation::GetBalance { account, block } if *block == BlockId::latest() => {
                Some(BalanceRead::Native { account: *account })
            }
            RpcOperation::EthCall {
                request,
                block,
                state_overrides,
            } if *block == BlockId::latest() && state_overrides.is_none() => {
                let TxKind::Call(token) = request.to? else {
                    return None;
                };
                let input = request.input.unique_input().ok()??;
                let call = RpcBrokerViewCalls::balanceOfCall::abi_decode_validate(input).ok()?;
                // Re-encoding excludes trailing bytes and noncanonical ABI words.
                if call.abi_encode().as_slice() != input.as_ref() {
                    return None;
                }
                Some(BalanceRead::Token {
                    account: call.account,
                    token,
                })
            }
            _ => None,
        }
    }

    pub(crate) const fn transaction_hash_lookup(&self) -> Option<(TransactionHashRead, B256)> {
        match &self.operation {
            RpcOperation::GetTransactionByHash(hash) => {
                Some((TransactionHashRead::Transaction, *hash))
            }
            RpcOperation::GetTransactionReceipt(hash) => {
                Some((TransactionHashRead::Receipt, *hash))
            }
            _ => None,
        }
    }

    /// Creates an `eth_call` read for the target contract and calldata.
    ///
    /// Defaults to `latest` with no sender, value, gas limit, or state overrides.
    #[must_use]
    pub fn eth_call(target: Address, calldata: Bytes) -> Self {
        let request = TransactionRequest {
            to: Some(target.into()),
            input: TransactionInput::maybe_both(Some(calldata)),
            ..TransactionRequest::default()
        };
        Self {
            operation: RpcOperation::EthCall {
                block: BlockId::latest(),
                request: Arc::new(WithOtherFields::new(request)),
                state_overrides: None,
            },
        }
    }
    /// Requests the account's native balance at `latest`.
    #[must_use]
    pub const fn get_balance(account: Address) -> Self {
        Self::get_balance_at(account, BlockId::latest())
    }

    #[must_use]
    pub(crate) const fn get_balance_at(account: Address, block: BlockId) -> Self {
        Self {
            operation: RpcOperation::GetBalance { account, block },
        }
    }

    #[must_use]
    pub(crate) const fn get_transaction_count(account: Address, block: BlockId) -> Self {
        Self {
            operation: RpcOperation::GetTransactionCount(account, block),
        }
    }

    #[must_use]
    pub(crate) const fn get_block_by_number(block: BlockNumberOrTag, full: bool) -> Self {
        Self {
            operation: RpcOperation::GetBlockByNumber(block, full),
        }
    }

    #[must_use]
    pub(crate) const fn get_transaction_by_block_hash_and_index(hash: B256, index: U256) -> Self {
        Self {
            operation: RpcOperation::GetTransactionByBlockHashAndIndex(hash, index),
        }
    }

    #[must_use]
    pub(crate) const fn get_block_receipts_by_hash(hash: B256) -> Self {
        Self {
            operation: RpcOperation::GetBlockReceipts(ReceiptBlock::Hash(hash)),
        }
    }

    #[must_use]
    pub(crate) const fn gas_price() -> Self {
        Self {
            operation: RpcOperation::GasPrice,
        }
    }

    #[must_use]
    pub(crate) const fn max_priority_fee_per_gas() -> Self {
        Self {
            operation: RpcOperation::MaxPriorityFeePerGas,
        }
    }

    #[must_use]
    pub(crate) const fn fee_history(
        count: U256,
        newest: BlockNumberOrTag,
        percentiles: Option<Vec<Number>>,
    ) -> Self {
        Self {
            operation: RpcOperation::FeeHistory(count, newest, percentiles),
        }
    }

    pub(crate) fn estimate_gas(
        transaction: WithOtherFields<TransactionRequest>,
        block: Option<BlockNumberOrTag>,
        route_chain_id: u64,
    ) -> Result<Self, RpcReadValidationError> {
        let read = Self::from_rpc(transaction, BlockId::latest(), None, route_chain_id)?;
        let RpcOperation::EthCall { request, .. } = read.operation else {
            unreachable!()
        };
        Ok(Self {
            operation: RpcOperation::EstimateGas(request, block),
        })
    }

    pub(crate) const fn method(&self) -> &'static str {
        self.operation.method()
    }

    #[must_use]
    pub(super) const fn reuse_block_id(&self) -> Option<BlockId> {
        match &self.operation {
            RpcOperation::EthCall { block, .. } | RpcOperation::GetBalance { block, .. } => {
                Some(*block)
            }
            _ => None,
        }
    }
    /// Estimates gas for batching, using an explicit call limit when provided.
    #[must_use]
    pub(super) fn estimated_gas(&self) -> u64 {
        match &self.operation {
            RpcOperation::EthCall { request, .. } => request.gas.unwrap_or(DEFAULT_READ_GAS),
            RpcOperation::GetBalance { .. } => 30_000,
            _ => DEFAULT_READ_GAS,
        }
    }
    #[cfg(test)]
    pub(super) fn with_test_block<B: Into<BlockId>>(mut self, block: B) -> Self {
        match &mut self.operation {
            RpcOperation::EthCall {
                block: selector, ..
            }
            | RpcOperation::GetBalance {
                block: selector, ..
            } => *selector = block.into(),
            _ => panic!("test block helper requires a call or balance"),
        }
        self
    }
    /// Parses the transaction and optional state override using Alloy's RPC types.
    ///
    /// `route_chain_id` is passed separately because chain scope belongs to the submission
    /// route, not to a read. Transaction fields follow Alloy's parsing semantics.
    pub fn from_rpc(
        transaction: WithOtherFields<TransactionRequest>,
        block: BlockId,
        state_overrides: Option<StateOverride>,
        route_chain_id: u64,
    ) -> Result<Self, RpcReadValidationError> {
        if transaction
            .chain_id
            .is_some_and(|chain| chain != route_chain_id)
        {
            return Err(RpcReadValidationError::ChainIdMismatch);
        }
        transaction
            .input
            .unique_input()
            .map_err(|_| RpcReadValidationError::InvalidParams)?;
        Ok(Self {
            operation: RpcOperation::EthCall {
                block,
                request: Arc::new(transaction),
                state_overrides,
            },
        })
    }

    /// Creates a read from an `eth_call` request's JSON `params` array.
    ///
    /// Accepts a call object, an optional block selection, and optional state overrides.
    /// The block defaults to `latest`. Block overrides are not supported.
    pub fn from_rpc_params(
        params: Value,
        route_chain_id: u64,
    ) -> Result<Self, RpcReadValidationError> {
        let Value::Array(params) = params else {
            return Err(RpcReadValidationError::InvalidParams);
        };
        if !(1..=3).contains(&params.len()) {
            return Err(RpcReadValidationError::InvalidParams);
        }
        let mut params = params.into_iter();
        let transaction_value = params.next().ok_or(RpcReadValidationError::InvalidParams)?;
        let block_value = params.next();
        let overrides_value = params.next();
        let block = match block_value {
            Some(block) => operation::typed(block)?,
            None => BlockId::latest(),
        };
        let overrides = overrides_value
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| RpcReadValidationError::InvalidParams)?;
        let transaction = serde_json::from_value(transaction_value)
            .map_err(|_| RpcReadValidationError::InvalidParams)?;
        Self::from_rpc(transaction, block, overrides, route_chain_id)
    }

    /// Parses a method from the broker's finite read allowlist. The route chain is checked
    /// here and again at submission. Parameters use Alloy's RPC types.
    pub fn from_method_params(
        method: &str,
        params: Value,
        route_chain_id: u64,
    ) -> Result<Self, RpcReadValidationError> {
        let Value::Array(params) = params else {
            return Err(RpcReadValidationError::InvalidParams);
        };
        Ok(Self {
            operation: RpcOperation::parse(method, params, route_chain_id)?,
        })
    }

    pub(super) fn result_from_abi(&self, bytes: &Bytes) -> Result<RpcResult, RpcBrokerError> {
        let value = match self.operation {
            RpcOperation::EthCall { .. } => serde_json::json!(bytes),
            RpcOperation::GetBalance { .. } if bytes.len() == 32 => {
                serde_json::json!(U256::from_be_slice(bytes))
            }
            _ => return Err(RpcBrokerError::InvalidResponse),
        };
        Ok(RpcResult::new(value))
    }

    pub(super) const fn operation(&self) -> &RpcOperation {
        &self.operation
    }

    /// Returns decoded variable-length input plus compact JSON bytes for transaction extras.
    ///
    /// Fixed transaction fields and state-override account keys are deliberately excluded.
    fn decoded_input_size(&self) -> Option<usize> {
        let (request, state_overrides) = match &self.operation {
            RpcOperation::EthCall {
                request,
                state_overrides,
                ..
            } => (request, state_overrides),
            operation => return operation.decoded_input_size(),
        };
        let mut size = transaction_input_size(request)?;
        if let Some(state_overrides) = state_overrides {
            for account in state_overrides.values() {
                if let Some(code) = account.code.as_ref() {
                    size = size.checked_add(code.len())?;
                }
                if let Some(state) = account.state.as_ref() {
                    size = size.checked_add(state.len().checked_mul(64)?)?;
                }
                if let Some(state_diff) = account.state_diff.as_ref() {
                    size = size.checked_add(state_diff.len().checked_mul(64)?)?;
                }
            }
        }
        Some(size)
    }
    #[must_use]
    pub(super) fn identity_for_route(&self, route: &RpcRoute) -> ReadIdentity {
        match &self.operation {
            RpcOperation::EthCall { request, block, .. } if request.other.is_empty() => {
                ReadIdentity::EthCall {
                    chain_id: route.chain_id(),
                    block: *block,
                    request: Arc::clone(request),
                }
            }
            RpcOperation::GetBalance { account, block } => ReadIdentity::GetBalance {
                chain_id: route.chain_id(),
                block: *block,
                account: *account,
            },
            _ => ReadIdentity::Individual {
                chain_id: route.chain_id(),
            },
        }
    }
    pub(super) fn is_cacheable(&self) -> bool {
        let Some(block) = self.reuse_block_id() else {
            return false;
        };
        let block_cacheable = match block {
            BlockId::Number(BlockNumberOrTag::Latest | BlockNumberOrTag::Number(_)) => true,
            BlockId::Hash(hash) => hash.require_canonical != Some(true),
            BlockId::Number(
                BlockNumberOrTag::Earliest
                | BlockNumberOrTag::Pending
                | BlockNumberOrTag::Safe
                | BlockNumberOrTag::Finalized,
            ) => false,
        };
        block_cacheable && self.is_dedupable()
    }

    pub(super) fn is_dedupable(&self) -> bool {
        matches!(self.operation(), RpcOperation::GetBalance { .. })
            || matches!(
                self.operation(),
                RpcOperation::EthCall {
                    request,
                    state_overrides,
                    ..
                } if request.value.unwrap_or_default() == U256::ZERO && state_overrides.is_none()
                    && !request.has_eip4844_fields() && request.authorization_list.is_none()
                    && request.other.is_empty()
            )
    }

    /// Assumes calls without a sender, and allowlisted selectors with a sender, tolerate
    /// Multicall3's caller identity and shared simulated state.
    #[must_use]
    pub(super) fn is_multicall_eligible(&self) -> bool {
        match &self.operation {
            RpcOperation::GetBalance { .. } => true,
            RpcOperation::EthCall {
                request,
                state_overrides,
                ..
            } => {
                if request.value.unwrap_or_default() != U256::ZERO
                    || !request.other.is_empty()
                    || state_overrides.is_some()
                    || request.gas.is_some()
                    || request.gas_price.is_some()
                    || request.chain_id.is_some()
                    || request.nonce.is_some()
                    || request.transaction_type.is_some()
                    || request.access_list.is_some()
                    || request.max_fee_per_gas.is_some()
                    || request.max_priority_fee_per_gas.is_some()
                    || request.has_eip4844_fields()
                    || request.authorization_list.is_some()
                    || !matches!(request.to, Some(TxKind::Call(_)))
                {
                    return false;
                }
                request.from.is_none_or(|_| {
                    request
                        .input
                        .input()
                        .and_then(|calldata| calldata.first_chunk::<4>())
                        .is_some_and(|selector| MSG_SENDER_INDEPENDENT.contains(selector))
                })
            }
            _ => false,
        }
    }
}

pub(super) fn transaction_input_size(
    request: &WithOtherFields<TransactionRequest>,
) -> Option<usize> {
    let mut size = match request.input.unique_input() {
        Ok(Some(input)) => input.len(),
        Ok(None) => 0,
        Err(_) => return None,
    };
    if let Some(access_list) = request.access_list.as_ref() {
        for item in access_list.iter() {
            size = size.checked_add(20)?;
            size = size.checked_add(item.storage_keys.len().checked_mul(32)?)?;
        }
    }
    if let Some(hashes) = &request.blob_versioned_hashes {
        size = size.checked_add(hashes.len().checked_mul(32)?)?;
    }
    if let Some(sidecar) = &request.sidecar {
        size = size.checked_add(sidecar.size())?;
    }
    if let Some(authorizations) = &request.authorization_list {
        for authorization in authorizations {
            size = size.checked_add(authorization.length())?;
        }
    }
    if !request.other.is_empty() {
        let mut counter = InputSizeCounter(0);
        serde_json::to_writer(&mut counter, &request.other).ok()?;
        size = size.checked_add(counter.0)?;
    }
    Some(size)
}

/// Counts compact JSON bytes without allocating another copy of transaction extras.
struct InputSizeCounter(usize);

impl std::io::Write for InputSizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_READ_INPUT_BYTES)
            .ok_or_else(|| std::io::Error::other("RPC input exceeds admission limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A method-preserving JSON result. Diagnostic formatting never exposes its payload.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcResult(Arc<Value>);

impl RpcResult {
    pub(super) fn new(value: Value) -> Self {
        Self(Arc::new(value))
    }

    /// Explicitly exposes the response payload to the caller. Do not log or persist it.
    #[must_use]
    pub fn expose_value(&self) -> &Value {
        &self.0
    }

    /// Takes the response payload for delivery to the authorized caller.
    #[must_use]
    pub fn into_value(self) -> Value {
        Arc::unwrap_or_clone(self.0)
    }
}

impl fmt::Debug for RpcResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RpcResult([REDACTED])")
    }
}

impl fmt::Display for RpcResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Keys the broker's cache and in-flight deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReadIdentity {
    // Individual work receives a unique WorkKey nonce and never enters the cache.
    Individual {
        chain_id: u64,
    },
    EthCall {
        chain_id: u64,
        block: BlockId,
        request: Arc<WithOtherFields<TransactionRequest>>,
    },
    GetBalance {
        chain_id: u64,
        block: BlockId,
        account: Address,
    },
}

impl Hash for ReadIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Individual { chain_id } => {
                2_u8.hash(state);
                chain_id.hash(state);
            }
            Self::EthCall {
                chain_id,
                block,
                request,
            } => {
                0_u8.hash(state);
                chain_id.hash(state);
                BlockKey(*block).hash(state);
                // Extra-bearing calls receive Individual identities and cannot be reused.
                request.inner.hash(state);
            }
            Self::GetBalance {
                chain_id,
                block,
                account,
            } => {
                1_u8.hash(state);
                chain_id.hash(state);
                BlockKey(*block).hash(state);
                account.hash(state);
            }
        }
    }
}

/// Adds hashing for Alloy's `BlockId`, which does not implement `Hash`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockKey(pub(super) BlockId);

impl Hash for BlockKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.0 {
            BlockId::Number(number) => {
                0_u8.hash(state);
                number.hash(state);
            }
            BlockId::Hash(hash) => {
                1_u8.hash(state);
                hash.block_hash.hash(state);
                hash.require_canonical.hash(state);
            }
        }
    }
}

impl ReadIdentity {
    pub(super) const fn chain_id(&self) -> u64 {
        match self {
            Self::EthCall { chain_id, .. }
            | Self::GetBalance { chain_id, .. }
            | Self::Individual { chain_id } => *chain_id,
        }
    }
    pub(super) const fn block(&self) -> Option<BlockId> {
        match self {
            Self::EthCall { block, .. } | Self::GetBalance { block, .. } => Some(*block),
            Self::Individual { .. } => None,
        }
    }
}

/// The fixed wallet subsystem identities used for broker scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalletRpcOrigin {
    Anchors,
    Governance,
    GovernorRewards,
    PublicWallet,
    Staking,
}

/// An authenticated source of broker work.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RpcOriginKind {
    Wallet(WalletRpcOrigin),
    Dapp { paired_peer_id: String, origin: Url },
}

/// An authenticated source of broker work.
///
/// The representation is intentionally opaque so callers cannot construct a dapp identity
/// without going through the validation performed by [`RpcOrigin::dapp`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RpcOrigin(RpcOriginKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RpcOriginError {
    #[error("invalid RPC origin")]
    InvalidOrigin,
    #[error("paired peer ID is empty")]
    EmptyPeerId,
}

impl From<WalletRpcOrigin> for RpcOrigin {
    fn from(origin: WalletRpcOrigin) -> Self {
        Self(RpcOriginKind::Wallet(origin))
    }
}

impl RpcOrigin {
    /// Builds a dapp identity from an authenticated paired peer and browser-attested URL.
    ///
    /// Accepts every URL parsed by [`Url::parse`] and retains the full URL with only the
    /// parser's normalization. The caller must supply the authenticated peer and
    /// browser-attested URL; parsing does not authenticate either or grant permission.
    pub fn dapp(paired_peer_id: impl Into<String>, origin: &str) -> Result<Self, RpcOriginError> {
        let paired_peer_id = paired_peer_id.into();
        if paired_peer_id.is_empty() {
            return Err(RpcOriginError::EmptyPeerId);
        }
        let url = Url::parse(origin).map_err(|_| RpcOriginError::InvalidOrigin)?;
        Ok(Self(RpcOriginKind::Dapp {
            paired_peer_id,
            origin: url,
        }))
    }

    /// Applies the gateway policy for HTTP(S) origins, using the URL parser's
    /// scheme, host, and effective port normalization. Opaque origins are rejected.
    pub fn dapp_web_origin(
        paired_peer_id: impl Into<String>,
        url: &str,
    ) -> Result<Self, RpcOriginError> {
        let origin = Url::parse(url)
            .map_err(|_| RpcOriginError::InvalidOrigin)?
            .origin();
        if !matches!(&origin, url::Origin::Tuple(scheme, _, _) if scheme == "http" || scheme == "https")
        {
            return Err(RpcOriginError::InvalidOrigin);
        }
        Self::dapp(paired_peer_id, &origin.ascii_serialization())
    }

    #[must_use]
    pub fn paired_peer_id(&self) -> Option<&str> {
        match &self.0 {
            RpcOriginKind::Dapp { paired_peer_id, .. } => Some(paired_peer_id),
            RpcOriginKind::Wallet(_) => None,
        }
    }

    /// Returns the full parsed dapp URL, including its path, query, and fragment.
    #[must_use]
    pub const fn web_origin(&self) -> Option<&Url> {
        match &self.0 {
            RpcOriginKind::Dapp { origin, .. } => Some(origin),
            RpcOriginKind::Wallet(_) => None,
        }
    }

    /// The per-submission read count cap that applies to this origin.
    ///
    /// Untrusted dapp submissions are capped; wallet subsystems submit their own planned work and
    /// are re-chunked by the actor, so only the byte caps apply to them.
    pub(super) const fn read_limit(&self) -> Option<usize> {
        match &self.0 {
            RpcOriginKind::Dapp { .. } => Some(MAX_SUBMISSION_READS),
            RpcOriginKind::Wallet(_) => None,
        }
    }
}

/// A clone-shared snapshot of chain endpoints and Multicall3 configuration.
///
/// Equality and hashing compare contents, so independently built equivalent snapshots share
/// deduplication keys.
#[derive(Debug, Clone)]
pub struct RpcChainRoute {
    chain_id: u64,
    endpoints: Arc<[SensitiveUrl]>,
    multicall: Option<Address>,
    verify_identity: bool,
    pub(super) verified_identities: Arc<[(SensitiveUrl, tokio::sync::OnceCell<()>)]>,
}

impl PartialEq for RpcChainRoute {
    fn eq(&self, other: &Self) -> bool {
        self.chain_id == other.chain_id
            && self.endpoints == other.endpoints
            && self.multicall == other.multicall
            && self.verify_identity == other.verify_identity
    }
}

impl Eq for RpcChainRoute {}

impl Hash for RpcChainRoute {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.chain_id.hash(state);
        self.multicall.hash(state);
        self.verify_identity.hash(state);
        for endpoint in self.endpoints.iter() {
            endpoint.expose_url().hash(state);
        }
    }
}

impl RpcChainRoute {
    /// Creates a chain route from a chain ID and RPC endpoints.
    #[must_use]
    pub fn new<E: Into<SensitiveUrl>>(chain_id: u64, endpoints: Vec<E>) -> Self {
        let endpoints: Arc<[SensitiveUrl]> = endpoints
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>()
            .into();
        let verified_identities = endpoints
            .iter()
            .map(|endpoint| (endpoint.clone(), tokio::sync::OnceCell::new()))
            .collect::<Vec<_>>()
            .into();
        Self {
            chain_id,
            endpoints,
            multicall: None,
            verify_identity: false,
            verified_identities,
        }
    }
    #[must_use]
    pub const fn with_identity_verification(mut self) -> Self {
        self.verify_identity = true;
        self
    }
    pub(crate) const fn requires_identity_verification(&self) -> bool {
        self.verify_identity
    }
    /// Restrict an admitted read while retaining this configuration's verified identities.
    pub(crate) fn for_endpoint(&self, endpoint: SensitiveUrl) -> Self {
        debug_assert!(self.endpoints.contains(&endpoint));
        let mut route = self.clone();
        route.endpoints = vec![endpoint].into();
        route
    }
    #[must_use]
    pub const fn with_multicall(mut self, address: Address) -> Self {
        self.multicall = Some(address);
        self
    }
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    #[must_use]
    pub fn endpoints(&self) -> &[SensitiveUrl] {
        &self.endpoints
    }
    #[must_use]
    pub fn endpoint_urls(&self) -> Vec<Url> {
        self.endpoints()
            .iter()
            .map(|endpoint| endpoint.expose_url().clone())
            .collect()
    }
    #[must_use]
    pub const fn multicall(&self) -> Option<Address> {
        self.multicall
    }
}

/// Per-submission execution policy paired with an immutable chain route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RpcRoute {
    pub(super) chain: RpcChainRoute,
    pub(super) max_calls: usize,
    pub(super) max_estimated_gas: u64,
    pub(super) request_timeout: Option<Duration>,
    pub(super) attempt_timeout: Duration,
}

impl From<RpcChainRoute> for RpcRoute {
    /// Creates an RPC route by pairing a chain route with the default execution policy.
    fn from(chain: RpcChainRoute) -> Self {
        Self {
            chain,
            max_calls: DEFAULT_MAX_CALLS,
            max_estimated_gas: DEFAULT_MAX_ESTIMATED_GAS,
            request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
        }
    }
}

impl RpcRoute {
    #[must_use]
    pub fn endpoints(&self) -> &[SensitiveUrl] {
        self.chain.endpoints()
    }
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain.chain_id()
    }
    #[must_use]
    pub const fn multicall(&self) -> Option<Address> {
        self.chain.multicall()
    }
    #[must_use]
    pub const fn chain_route(&self) -> &RpcChainRoute {
        &self.chain
    }
    #[cfg(test)]
    #[must_use]
    pub(super) fn with_test_thresholds(mut self, max_calls: usize, max_estimated_gas: u64) -> Self {
        self.max_calls = max_calls.max(1);
        self.max_estimated_gas = max_estimated_gas;
        self
    }
    #[must_use]
    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }
    #[must_use]
    pub const fn with_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = if timeout.is_zero() {
            DEFAULT_ATTEMPT_TIMEOUT
        } else {
            timeout
        };
        self
    }
}

/// An ordered group of reads with shared routing policy and caller origin.
#[derive(Debug, Clone)]
pub struct RpcSubmission {
    route: RpcRoute,
    reads: Vec<RpcRead>,
    origin: RpcOrigin,
}

impl RpcSubmission {
    #[must_use]
    pub const fn new(route: RpcRoute, reads: Vec<RpcRead>, origin: RpcOrigin) -> Self {
        Self {
            route,
            reads,
            origin,
        }
    }
    #[must_use]
    pub const fn route(&self) -> &RpcRoute {
        &self.route
    }
    #[must_use]
    pub fn reads(&self) -> &[RpcRead] {
        &self.reads
    }
    #[must_use]
    pub const fn origin(&self) -> &RpcOrigin {
        &self.origin
    }
    pub(super) fn into_parts(self) -> (RpcRoute, Vec<RpcRead>, RpcOrigin) {
        (self.route, self.reads, self.origin)
    }

    pub(super) fn validate_admission(&self) -> Result<(), RpcBrokerError> {
        if self
            .origin
            .read_limit()
            .is_some_and(|limit| self.reads.len() > limit)
        {
            return Err(RpcBrokerError::AdmissionRejected);
        }
        let mut total_size = 0_usize;
        for read in &self.reads {
            let request = match read.operation() {
                RpcOperation::EthCall { request, .. } | RpcOperation::EstimateGas(request, _) => {
                    Some(request)
                }
                operation
                    if operation.exact_transaction_hash()
                        && matches!(self.origin.0, RpcOriginKind::Wallet(_)) =>
                {
                    return Err(RpcBrokerError::OriginRejected);
                }
                _ => None,
            };
            if request.is_some_and(|request| {
                request
                    .chain_id
                    .is_some_and(|chain| chain != self.route.chain_id())
            }) {
                return Err(RpcBrokerError::InvalidRead(
                    RpcReadValidationError::ChainIdMismatch,
                ));
            }
            let Some(read_size) = read.decoded_input_size() else {
                return Err(RpcBrokerError::AdmissionRejected);
            };
            if read_size > MAX_READ_INPUT_BYTES {
                return Err(RpcBrokerError::AdmissionRejected);
            }
            total_size = total_size
                .checked_add(read_size)
                .ok_or(RpcBrokerError::AdmissionRejected)?;
            if total_size > MAX_SUBMISSION_INPUT_BYTES {
                return Err(RpcBrokerError::AdmissionRejected);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum RpcBrokerSpawnError {
    #[error("RPC submission capacity exceeds the supported semaphore permit limit")]
    InvalidSubmissionCapacity,
    #[error("no active Tokio runtime")]
    NoRuntime(#[source] tokio::runtime::TryCurrentError),
    #[error("RPC broker actor failed to start")]
    ActorStartup,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RpcBrokerError {
    #[error("RPC operation is not permitted for this origin")]
    OriginRejected,
    #[error("invalid RPC read: {0}")]
    InvalidRead(RpcReadValidationError),
    #[error("RPC response exceeds the local resource limit")]
    ResponseTooLarge,
    #[error("RPC submission exceeds admission limits")]
    AdmissionRejected,
    #[error("RPC broker is shut down")]
    Shutdown,
    #[error("no configured RPC endpoint is available for chain {chain_id}")]
    NoEndpoint { chain_id: u64 },
    #[error("RPC request failed ({0})")]
    Remote(RpcRemoteError),
    #[error("RPC endpoint returned HTTP status {0}")]
    HttpStatus(u16),
    #[error("RPC read reverted")]
    InnerRevert(RpcRevert),
    #[error("RPC request timed out")]
    Timeout,
    #[error("RPC request deadline elapsed before dispatch")]
    TimeoutBeforeDispatch,
    #[error("RPC transport failed")]
    Transport,
    #[error("invalid RPC response")]
    InvalidResponse,
}

impl RpcBrokerError {
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Remote(remote) => JsonRpcFailurePolicy::from(remote.code()).failure_class(),
            Self::Timeout | Self::TimeoutBeforeDispatch | Self::Transport => {
                FailureClass::Transient
            }
            Self::HttpStatus(status) if is_transient_http_status(*status) => {
                FailureClass::Transient
            }
            _ => FailureClass::Unrecoverable,
        }
    }
}

/// Returns the first member error when no member succeeded or reverted, i.e. when the whole
/// submission failed at the transport/JSON-RPC layer rather than at the target contract.
pub(crate) fn total_failure<T>(results: &[Result<T, RpcBrokerError>]) -> Option<&RpcBrokerError> {
    if results
        .iter()
        .any(|result| matches!(result, Ok(_) | Err(RpcBrokerError::InnerRevert(_))))
    {
        return None;
    }
    results.iter().find_map(|result| result.as_ref().err())
}

/// The structured JSON-RPC error returned by a remote endpoint.
///
/// The payload is retained for callers that explicitly cross the sensitive-data boundary, but
/// formatting this value only exposes its numeric error code.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcRemoteError(Arc<WithOtherFields<ErrorPayload<Value>>>);

impl From<WithOtherFields<ErrorPayload<Value>>> for RpcRemoteError {
    fn from(payload: WithOtherFields<ErrorPayload<Value>>) -> Self {
        Self(Arc::new(payload))
    }
}

impl RpcRemoteError {
    #[must_use]
    pub fn code(&self) -> i64 {
        self.0.inner.code
    }

    #[must_use]
    pub fn expose_message(&self) -> &str {
        self.0.inner.message.as_ref()
    }

    #[must_use]
    pub fn expose_data(&self) -> Option<&Value> {
        self.0.inner.data.as_ref()
    }

    #[must_use]
    pub fn expose_payload(&self) -> &WithOtherFields<ErrorPayload<Value>> {
        self.0.as_ref()
    }
}

impl fmt::Debug for RpcRemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcRemoteError")
            .field("code", &self.code())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RpcRemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.code())
    }
}

/// A reverted RPC read and, when available, the structured remote error that supplied it.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcRevert {
    bytes: Bytes,
    source: Option<RpcRemoteError>,
}

impl RpcRevert {
    pub(crate) const fn from_individual(bytes: Bytes, source: RpcRemoteError) -> Self {
        Self {
            bytes,
            source: Some(source),
        }
    }

    pub(crate) const fn from_multicall(bytes: Bytes) -> Self {
        Self {
            bytes,
            source: None,
        }
    }

    #[must_use]
    pub const fn expose_bytes(&self) -> &Bytes {
        &self.bytes
    }

    #[must_use]
    pub const fn source(&self) -> Option<&RpcRemoteError> {
        self.source.as_ref()
    }
}

impl fmt::Debug for RpcRevert {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcRevert")
            .field(
                "source_code",
                &self.source.as_ref().map(RpcRemoteError::code),
            )
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RpcRevert {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RPC read reverted")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JsonRpcFailurePolicy {
    Reducible,
    TransientStrike,
    UnrecoverableStrike,
    UnrecoverableNeutral,
}

impl JsonRpcFailurePolicy {
    const fn failure_class(self) -> FailureClass {
        match self {
            Self::Reducible => FailureClass::RecoverableByReduction,
            Self::TransientStrike => FailureClass::Transient,
            Self::UnrecoverableStrike | Self::UnrecoverableNeutral => FailureClass::Unrecoverable,
        }
    }
}

impl From<i64> for JsonRpcFailurePolicy {
    fn from(code: i64) -> Self {
        match code {
            -32000 | -32016 => Self::Reducible,
            -32005 => Self::TransientStrike,
            -32601 | -32603 => Self::UnrecoverableStrike,
            _ => Self::UnrecoverableNeutral,
        }
    }
}

const fn is_transient_http_status(status: u16) -> bool {
    status == 429 || (status >= 500 && status <= 599)
}

/// Controls reduction and failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    RecoverableByReduction,
    Transient,
    Unrecoverable,
}
