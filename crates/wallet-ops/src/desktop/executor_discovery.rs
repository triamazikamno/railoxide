use std::collections::BTreeMap;
use std::time::Duration;

use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::executor::EXECUTION_NONCE_STORAGE_SLOT;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};

use crate::HttpContext;
use crate::public_wallet::PublicErc20;
use crate::settings::{EffectiveChainConfig, ExecutorProfile};

// The pinned shared bindings have no NFT reads. Match the standard IERC721 API
// used by the accepted RelayAdapt7702; ERC1155 shielding is explicitly unsupported there.
alloy::sol! {
    interface ExecutorErc721 {
        event Approval(address indexed owner, address indexed approved, uint256 indexed tokenId);
        function ownerOf(uint256 tokenId) external view returns (address);
        function getApproved(uint256 tokenId) external view returns (address);
        function approve(address spender, uint256 tokenId) external;
    }
}

pub use crate::vault::ExecutorAsset;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorActivity {
    PreviouslyUsed,
    Unknown,
    /// Only the requested asset set was checked; this is not a whole-family history claim.
    NoObservedActivity,
}

#[derive(Clone)]
pub struct ExecutorInspection {
    chain_id: u64,
    address: Address,
    block: BlockNumHash,
    account_nonce: Option<u64>,
    code: Option<Bytes>,
    execution_nonce: Option<U256>,
    balances: BTreeMap<ExecutorAsset, Option<U256>>,
}

impl ExecutorInspection {
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    #[must_use]
    pub const fn address(&self) -> Address {
        self.address
    }
    #[must_use]
    pub const fn block(&self) -> BlockNumHash {
        self.block
    }
    #[must_use]
    pub const fn account_nonce(&self) -> Option<u64> {
        self.account_nonce
    }
    #[must_use]
    pub const fn execution_nonce(&self) -> Option<U256> {
        self.execution_nonce
    }
    #[must_use]
    pub fn code(&self) -> Option<&[u8]> {
        self.code.as_ref().map(AsRef::as_ref)
    }
    #[must_use]
    pub const fn balances(&self) -> &BTreeMap<ExecutorAsset, Option<U256>> {
        &self.balances
    }

    #[must_use]
    pub fn activity(&self) -> ExecutorActivity {
        if self.account_nonce.is_some_and(|nonce| nonce != 0)
            || self.code.as_ref().is_some_and(|code| !code.is_empty())
            || self
                .balances
                .values()
                .flatten()
                .any(|balance| !balance.is_zero())
        {
            ExecutorActivity::PreviouslyUsed
        } else if self.account_nonce.is_none()
            || self.code.is_none()
            || self.balances.values().any(Option::is_none)
        {
            ExecutorActivity::Unknown
        } else {
            ExecutorActivity::NoObservedActivity
        }
    }

    #[must_use]
    pub fn has_incomplete_reads(&self) -> bool {
        self.account_nonce.is_none()
            || self.code.is_none()
            || self.execution_nonce.is_none()
            || self.balances.values().any(Option::is_none)
    }
}

/// User-directed inspection of one executor. It never scans another derived address.
/// `None` balances mean a failed read, and the asset set never proves exhaustive discovery.
pub async fn inspect_executor(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<ExecutorInspection> {
    inspect_executor_state(chain, http, address, assets, false).await
}

async fn inspect_executor_state(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
    recovery: bool,
) -> Result<ExecutorInspection> {
    let pool = QueryRpcPool::with_http_client(
        chain.rpc_route.endpoint_urls(),
        Duration::from_secs(30),
        http.rpc_client.clone(),
    );
    for provider in pool.available_providers() {
        // Avoid leaking the selected address to an endpoint serving a different chain.
        if provider.provider.get_chain_id().await.ok() != Some(chain.chain_id) {
            continue;
        }
        let Ok(Some(head)) = provider
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
        else {
            continue;
        };
        return Ok(inspect_at(
            &provider.provider,
            chain,
            address,
            assets,
            head.header.num_hash(),
            recovery,
        )
        .await);
    }
    Err(eyre!("executor chain state is unavailable"))
}

/// Recovery explicitly installs the recorded supported profile. Read its storage
/// layout even when the account currently delegates elsewhere; never call that code.
pub(super) async fn inspect_recovery_executor(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<ExecutorInspection> {
    inspect_executor_state(chain, http, address, assets, true).await
}

pub(super) async fn inspect_at(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    assets: &[ExecutorAsset],
    block: BlockNumHash,
    recovery: bool,
) -> ExecutorInspection {
    let block_id = BlockId::hash_canonical(block.hash);
    let account_nonce = provider
        .get_transaction_count(address)
        .block_id(block_id)
        .await
        .ok();
    let code = provider.get_code_at(address).block_id(block_id).await.ok();
    let mut balances = BTreeMap::new();
    balances.insert(
        ExecutorAsset::Native,
        provider.get_balance(address).block_id(block_id).await.ok(),
    );
    for asset in assets {
        if balances.contains_key(asset) {
            continue;
        }
        let balance = match asset {
            ExecutorAsset::Native => continue,
            ExecutorAsset::Erc20(token) => {
                call(
                    provider,
                    *token,
                    PublicErc20::balanceOfCall { account: address },
                    block_id,
                )
                .await
            }
            ExecutorAsset::Erc721 {
                collection,
                token_id,
            } => call(
                provider,
                *collection,
                ExecutorErc721::ownerOfCall { tokenId: *token_id },
                block_id,
            )
            .await
            .map(|owner| U256::from(u8::from(owner == address))),
        };
        balances.insert(*asset, balance);
    }
    let execution_nonce = execution_nonce_with_code(
        provider,
        chain,
        address,
        block_id,
        code.as_ref().map(AsRef::as_ref),
        recovery,
    )
    .await;
    ExecutorInspection {
        chain_id: chain.chain_id,
        address,
        block,
        account_nonce,
        code,
        execution_nonce,
        balances,
    }
}

/// Read only the execution nonce when balances and the Ethereum account nonce
/// are not needed. The code read selects the accepted delegate's nonce layout.
pub(super) async fn execution_nonce_at(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block: BlockNumHash,
) -> Option<U256> {
    let block_id = BlockId::hash_canonical(block.hash);
    let code = provider
        .get_code_at(address)
        .block_id(block_id)
        .await
        .ok()?;
    execution_nonce_with_code(provider, chain, address, block_id, Some(&code), false).await
}

async fn execution_nonce_with_code(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block_id: BlockId,
    code: Option<&[u8]>,
    recovery: bool,
) -> Option<U256> {
    // Ordinary inspection never interprets another delegate's storage. Recovery
    // explicitly installs this supported profile and must preserve its stored nonce.
    let profile = chain.railgun.as_ref().and_then(|railgun| {
        ExecutorProfile::accepted(chain.chain_id, railgun.deployment.relay_adapt_7702_contract)
    });
    match (profile, code) {
        (Some(_), Some(code))
            if code.is_empty() || recovery && executor_delegation(code).is_some() =>
        {
            provider
                .get_storage_at(address, EXECUTION_NONCE_STORAGE_SLOT)
                .block_id(block_id)
                .await
                .ok()
        }
        (Some(profile), Some(code)) if matches_executor_delegation(code, profile) => {
            call(provider, address, RelayAdapt7702::nonceCall {}, block_id).await
        }
        _ => None,
    }
}

/// Signing requires the confirmed execution nonce to remain current at the tip.
/// Account nonce and delegation come from the tip, not the older confirmed block.
pub(super) async fn inspect_for_signing(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<(ExecutorInspection, crate::vault::ExecutorNonceObservation)> {
    let (inspection, observed) = Box::pin(inspect_for_recovery_signing(
        chain, http, address, assets, true, None,
    ))
    .await?;
    Ok((
        inspection,
        observed.ok_or_else(|| eyre!("executor execution nonce is unknown"))?,
    ))
}

/// Ordinary recovery may originate from an EIP-7702 account with another delegate.
/// Its owner separately requires a known execution nonce when issued payloads exist.
pub(super) async fn inspect_for_recovery_signing(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
    require_execution_nonce: bool,
    replacement_nonce: Option<u64>,
) -> Result<(
    ExecutorInspection,
    Option<crate::vault::ExecutorNonceObservation>,
)> {
    inspect_signing_state(
        chain,
        http,
        address,
        assets,
        require_execution_nonce,
        replacement_nonce,
        false,
    )
    .await
}

pub(super) async fn inspect_for_recovery_batch(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
    replacement_nonce: Option<u64>,
) -> Result<(ExecutorInspection, crate::vault::ExecutorNonceObservation)> {
    let (inspection, observed) =
        inspect_signing_state(chain, http, address, assets, true, replacement_nonce, true).await?;
    Ok((
        inspection,
        observed.ok_or_else(|| eyre!("recovery execution nonce is unknown"))?,
    ))
}

async fn inspect_signing_state(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    address: Address,
    assets: &[ExecutorAsset],
    require_execution_nonce: bool,
    replacement_nonce: Option<u64>,
    recovery: bool,
) -> Result<(
    ExecutorInspection,
    Option<crate::vault::ExecutorNonceObservation>,
)> {
    let pool = QueryRpcPool::with_http_client(
        chain.rpc_route.endpoint_urls(),
        Duration::from_secs(30),
        http.rpc_client.clone(),
    );
    for endpoint in pool.available_providers() {
        let provider = &endpoint.provider;
        if provider.get_chain_id().await.ok() != Some(chain.chain_id) {
            continue;
        }
        let Ok(Some(head)) = provider.get_block_by_number(BlockNumberOrTag::Latest).await else {
            continue;
        };
        let head = head.header.num_hash();
        let confirmed_number = head.number.saturating_sub(chain.finality_depth);
        let Ok(Some(confirmed)) = provider.get_block_by_number(confirmed_number.into()).await
        else {
            continue;
        };
        let confirmed = confirmed.header.num_hash();
        let current = inspect_at(provider, chain, address, assets, head, recovery).await;
        if current.account_nonce().is_none()
            || current.code().is_none()
            || current.balances().values().any(Option::is_none)
            || require_execution_nonce && current.execution_nonce().is_none()
        {
            continue;
        }
        let nonce = if confirmed == head {
            current.execution_nonce()
        } else if recovery {
            // The reviewed target layout persists across delegation changes, including
            // at the older block. Its runtime need not be installed at that block.
            provider
                .get_storage_at(address, EXECUTION_NONCE_STORAGE_SLOT)
                .block_id(BlockId::hash_canonical(confirmed.hash))
                .await
                .ok()
        } else {
            execution_nonce_at(provider, chain, address, confirmed).await
        };
        if require_execution_nonce && nonce.is_none() {
            continue;
        }
        if current.execution_nonce() != nonce {
            return Err(eyre!("executor execution nonce is awaiting confirmation"));
        }
        let Ok(pending_nonce) = provider.get_transaction_count(address).pending().await else {
            continue;
        };
        if current.account_nonce() != Some(pending_nonce)
            && replacement_nonce != current.account_nonce()
        {
            return Err(eyre!(
                "executor account has an unconfirmed transaction or authorization"
            ));
        }
        let Ok(Some(canonical)) = provider.get_block_by_number(head.number.into()).await else {
            continue;
        };
        if canonical.header.num_hash() != head {
            continue;
        }
        return Ok((
            current,
            nonce.map(|nonce| crate::vault::ExecutorNonceObservation::new(confirmed, nonce)),
        ));
    }
    Err(eyre!("executor signing state is unavailable"))
}

async fn call<C: SolCall>(
    provider: &DynProvider,
    to: Address,
    call: C,
    block: BlockId,
) -> Option<C::Return> {
    let bytes = provider
        .call(
            TransactionRequest::default()
                .to(to)
                .input(call.abi_encode().into()),
        )
        .block(block)
        .await
        .ok()?;
    C::abi_decode_returns(&bytes).ok()
}

pub(super) fn executor_delegation(code: &[u8]) -> Option<Address> {
    // Alloy 2.3 exposes the designator but has no decoder for account code.
    Address::try_from(code.strip_prefix(&EIP7702_DELEGATION_DESIGNATOR)?).ok()
}

pub(crate) fn matches_executor_delegation(code: &[u8], profile: ExecutorProfile) -> bool {
    // Alloy 2.3 provides the designator constant but no account-code decoder.
    // Compare the accepted EIP-7702 code directly; do not accept arbitrary code as delegation.
    code.strip_prefix(&EIP7702_DELEGATION_DESIGNATOR) == Some(profile.delegate().as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{WalletSettings, build_effective_chain_configs};
    use alloy::primitives::B256;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;

    #[tokio::test]
    async fn executor_inspection_uses_the_context_proxy_and_checks_chain_before_address_reads() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let proxy = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let rpc_client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).unwrap())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let http =
            HttpContext::with_rpc_client_for_tests(rpc_client, crate::WalletNetworkMode::Proxy);
        let proxy_request = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut first = String::new();
            socket.read_line(&mut first).await.unwrap();
            let mut length = None;
            loop {
                let mut header = String::new();
                assert_ne!(socket.read_line(&mut header).await.unwrap(), 0);
                if header == "\r\n" {
                    break;
                }
                if let Some((key, value)) = header.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.unwrap()];
            socket.read_exact(&mut body).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let response =
                serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x38"})
                    .to_string();
            socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            (first, request)
        });
        let mut chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(
            1,
            vec![
                "http://executor-rpc.invalid/private-route"
                    .parse::<url::Url>()
                    .unwrap(),
            ],
        );
        let inspected = tokio::time::timeout(
            Duration::from_secs(3),
            inspect_executor(
                &chain,
                &http,
                Address::repeat_byte(1),
                &[ExecutorAsset::Native],
            ),
        )
        .await
        .unwrap();
        assert!(inspected.is_err());
        let (first, request) = tokio::time::timeout(Duration::from_secs(3), proxy_request)
            .await
            .unwrap()
            .unwrap();
        assert!(first.starts_with("POST http://executor-rpc.invalid/private-route "));
        assert_eq!(request["method"], "eth_chainId");
        assert!(
            request
                .get("params")
                .is_none_or(|params| params.as_array().is_some_and(Vec::is_empty))
        );
    }

    #[tokio::test]
    async fn executor_inspection_preserves_partial_results_without_treating_failed_reads_as_empty()
    {
        let chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let address = Address::repeat_byte(1);
        let token = ExecutorAsset::Erc20(Address::repeat_byte(2));
        let nft = ExecutorAsset::Erc721 {
            collection: Address::repeat_byte(3),
            token_id: U256::from(4),
        };
        let block = BlockNumHash::new(10, B256::repeat_byte(10));
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        responses.push_success(&"0x0");
        responses.push_success(&Bytes::new());
        responses.push_success(&"0x0");
        responses.push_failure_msg("token read unavailable");
        responses.push_failure_msg("nonce read unavailable");
        let partial = inspect_at(&provider, &chain, address, &[token], block, false).await;
        assert_eq!(partial.activity(), ExecutorActivity::Unknown);
        assert!(partial.has_incomplete_reads());
        assert_eq!(partial.balances()[&ExecutorAsset::Native], Some(U256::ZERO));
        assert_eq!(partial.balances()[&token], None);
        assert_eq!(partial.execution_nonce(), None);

        responses.push_success(&"0x0");
        responses.push_success(&Bytes::new());
        responses.push_success(&"0x0");
        responses.push_failure_msg("token read unavailable");
        responses.push_success(&Bytes::from(
            ExecutorErc721::ownerOfCall::abi_encode_returns(&address),
        ));
        responses.push_success(&"0x0");
        let with_nft = inspect_at(&provider, &chain, address, &[token, nft], block, false).await;
        assert_eq!(with_nft.activity(), ExecutorActivity::PreviouslyUsed);
        assert!(with_nft.has_incomplete_reads());
        assert_eq!(with_nft.balances()[&nft], Some(U256::from(1)));
    }

    #[tokio::test]
    async fn executor_inspection_reads_execution_nonce_only_for_the_accepted_delegation() {
        let mut chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let profile = chain.accepted_executor_profile().unwrap();
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let address = Address::repeat_byte(1);
        let block = BlockNumHash::new(10, B256::repeat_byte(10));
        for delegate in [profile.delegate(), Address::repeat_byte(2)] {
            responses.push_success(&"0x2");
            responses.push_success(&Bytes::from(
                [
                    EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                    delegate.as_slice(),
                ]
                .concat(),
            ));
            responses.push_success(&"0x0");
            if delegate == profile.delegate() {
                responses.push_success(&Bytes::from(
                    RelayAdapt7702::nonceCall::abi_encode_returns(&U256::from(8)),
                ));
            }
            let inspection = inspect_at(&provider, &chain, address, &[], block, false).await;
            assert_eq!(inspection.activity(), ExecutorActivity::PreviouslyUsed);
            assert_eq!(inspection.account_nonce(), Some(2));
            assert_eq!(
                inspection.execution_nonce(),
                (delegate == profile.delegate()).then_some(U256::from(8))
            );
        }
        // A historical executor can retain a consumed nonce with no delegation.
        // Disabling new execution must preserve this recovery observation.
        chain.enabled = false;
        responses.push_success(&"0x5");
        responses.push_success(&Bytes::new());
        responses.push_success(&"0x0");
        responses.push_success(&"0x8");
        let revoked = inspect_at(&provider, &chain, address, &[], block, false).await;
        assert_eq!(revoked.execution_nonce(), Some(U256::from(8)));
        assert!(!revoked.has_incomplete_reads());

        // Nonce-only consumers must retain the stored nonce after revocation,
        // without fetching the unrelated account nonce or balances again.
        responses.push_success(&Bytes::new());
        responses.push_success(&"0x8");
        assert_eq!(
            execution_nonce_at(&provider, &chain, address, block).await,
            Some(U256::from(8)),
        );
    }
    #[tokio::test]
    async fn recovery_reads_target_nonce_without_calling_an_unrecognized_delegate() {
        let chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let source = Address::repeat_byte(1);
        let other = Address::repeat_byte(2);
        let designator =
            Bytes::from([EIP7702_DELEGATION_DESIGNATOR.as_slice(), other.as_slice()].concat());
        let block = BlockNumHash::new(10, B256::repeat_byte(10));
        // Recovery must use retained storage, not an arbitrary delegate's nonce() ABI.
        responses.push_success(&"0x2");
        responses.push_success(&designator);
        responses.push_success(&"0x10");
        responses.push_success(&"0x8");
        let recovery = inspect_at(&provider, &chain, source, &[], block, true).await;
        assert_eq!(recovery.execution_nonce(), Some(U256::from(8)));
        assert_eq!(executor_delegation(recovery.code().unwrap()), Some(other));

        // Arbitrary runtime code cannot be authorized away as an EIP-7702 delegation.
        responses.push_success(&"0x2");
        responses.push_success(&Bytes::from_static(&[0x60, 0x00]));
        responses.push_success(&"0x10");
        let contract = inspect_at(&provider, &chain, source, &[], block, true).await;
        assert_eq!(contract.execution_nonce(), None);
    }
}
