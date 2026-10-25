use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use alloy::transports::TransportResult;
use broadcaster_core::contracts::executor::EXECUTION_NONCE_STORAGE_SLOT;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, eyre};
use futures_util::future::{join, join_all, join3};

use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::public_wallet::PublicErc20;
use crate::rpc_broker::EndpointHealthOutcome;
use crate::settings::{EffectiveChainConfig, ExecutorProfile};
use crate::{HttpContext, OperationHttpClient};

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

/// A failed chain check, which is the caller's own check and not the endpoint's failure.
pub(super) const WRONG_CHAIN: &str = "the RPC endpoint serves another chain";

/// A read's value, `None` when it is unavailable, and the endpoint health outcome of the
/// requests it sent.
type Read<T> = (Option<T>, EndpointHealthOutcome);

/// An answered request is healthy, whatever the caller makes of its value.
fn answered<T>(result: TransportResult<T>) -> Read<T> {
    match result {
        Ok(value) => (Some(value), EndpointHealthOutcome::Healthy),
        Err(error) => (None, EndpointHealthOutcome::for_direct_error(&error)),
    }
}

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
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<ExecutorInspection> {
    inspect_executor_state(chain, http, client, address, assets, false).await
}

async fn inspect_executor_state(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
    recovery: bool,
) -> Result<ExecutorInspection> {
    let endpoints =
        ObservationEndpoints::new(chain, http, crate::RpcTrafficClass::Sensitive, client)?;
    let mut attempts = endpoints.attempts();
    while let Some(provider) = attempts.next().await {
        let head = async {
            // Avoid leaking the selected address to an endpoint serving a different chain.
            if provider.provider.get_chain_id().await? != chain.chain_id {
                return Err(eyre!(WRONG_CHAIN));
            }
            provider
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or_else(|| eyre!("latest executor block is unavailable"))
        }
        .await;
        let head = match head {
            Ok(head) => head,
            Err(error) => {
                endpoints.failed(&provider, &error);
                continue;
            }
        };
        let (inspection, outcome, ()) = inspect_at_with(
            &provider.provider,
            chain,
            address,
            assets,
            head.header.num_hash(),
            recovery,
            std::future::ready(()),
        )
        .await;
        // Partial results are still returned, without trying another endpoint.
        endpoints.report(&provider, outcome);
        return Ok(inspection);
    }
    Err(eyre!("executor chain state is unavailable"))
}

/// Recovery explicitly installs the recorded supported profile. Read its storage
/// layout even when the account currently delegates elsewhere; never call that code.
pub(super) async fn inspect_recovery_executor(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<ExecutorInspection> {
    inspect_executor_state(chain, http, client, address, assets, true).await
}

#[cfg(test)]
pub(super) async fn inspect_at(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    assets: &[ExecutorAsset],
    block: BlockNumHash,
    recovery: bool,
) -> ExecutorInspection {
    inspect_at_with(
        provider,
        chain,
        address,
        assets,
        block,
        recovery,
        std::future::ready(()),
    )
    .await
    .0
}

/// Runs `alongside` with the execution nonce read, after the account, code and
/// balance reads. The lazy future sends none of its requests before then. The outcome
/// combines every inspection request's, not `alongside`'s.
async fn inspect_at_with<T>(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    assets: &[ExecutorAsset],
    block: BlockNumHash,
    recovery: bool,
    alongside: impl Future<Output = T>,
) -> (ExecutorInspection, EndpointHealthOutcome, T) {
    let block_id = BlockId::hash_canonical(block.hash);
    // Native first, each asset once: requests keep the sequential read order.
    let mut seen = BTreeSet::new();
    let assets: Vec<_> = std::iter::once(ExecutorAsset::Native)
        .chain(assets.iter().copied())
        .filter(|asset| seen.insert(*asset))
        .collect();
    let ((account_nonce, account_outcome), (code, code_outcome), asset_balances) = join3(
        async {
            answered(
                provider
                    .get_transaction_count(address)
                    .block_id(block_id)
                    .await,
            )
        },
        async { answered(provider.get_code_at(address).block_id(block_id).await) },
        join_all(
            assets
                .iter()
                .map(|asset| read_asset_balance(provider, address, *asset, block_id)),
        ),
    )
    .await;
    let outcome = asset_balances.iter().fold(
        account_outcome.combine(code_outcome),
        |outcome, (_, balance)| outcome.combine(*balance),
    );
    let balances = assets
        .into_iter()
        .zip(asset_balances.into_iter().map(|(balance, _)| balance))
        .collect();
    let ((execution_nonce, nonce_outcome), alongside) = join(
        read_execution_nonce(
            provider,
            chain,
            address,
            block_id,
            code.as_ref().map(AsRef::as_ref),
            recovery,
        ),
        alongside,
    )
    .await;
    (
        ExecutorInspection {
            chain_id: chain.chain_id,
            address,
            block,
            account_nonce,
            code,
            execution_nonce,
            balances,
        },
        outcome.combine(nonce_outcome),
        alongside,
    )
}

pub(super) async fn asset_balance(
    provider: &DynProvider,
    address: Address,
    asset: ExecutorAsset,
    block_id: BlockId,
) -> Option<U256> {
    read_asset_balance(provider, address, asset, block_id)
        .await
        .0
}

async fn read_asset_balance(
    provider: &DynProvider,
    address: Address,
    asset: ExecutorAsset,
    block_id: BlockId,
) -> Read<U256> {
    match asset {
        ExecutorAsset::Native => answered(provider.get_balance(address).block_id(block_id).await),
        ExecutorAsset::Erc20(token) => {
            call(
                provider,
                token,
                PublicErc20::balanceOfCall { account: address },
                block_id,
            )
            .await
        }
        ExecutorAsset::Erc721 {
            collection,
            token_id,
        } => {
            let (owner, outcome) = call(
                provider,
                collection,
                ExecutorErc721::ownerOfCall { tokenId: token_id },
                block_id,
            )
            .await;
            (
                owner.map(|owner| U256::from(u8::from(owner == address))),
                outcome,
            )
        }
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
    read_execution_nonce_at(provider, chain, address, block)
        .await
        .0
}

async fn read_execution_nonce_at(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block: BlockNumHash,
) -> Read<U256> {
    let block_id = BlockId::hash_canonical(block.hash);
    match provider.get_code_at(address).block_id(block_id).await {
        Ok(code) => {
            read_execution_nonce(provider, chain, address, block_id, Some(&code), false).await
        }
        Err(error) => (None, EndpointHealthOutcome::for_direct_error(&error)),
    }
}

pub(super) async fn execution_nonce_with_code(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block_id: BlockId,
    code: Option<&[u8]>,
    recovery: bool,
) -> Option<U256> {
    read_execution_nonce(provider, chain, address, block_id, code, recovery)
        .await
        .0
}

/// Without an accepted nonce layout for `code`, or without `code`, sends nothing: the nonce is
/// `None` and the outcome healthy.
async fn read_execution_nonce(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    address: Address,
    block_id: BlockId,
    code: Option<&[u8]>,
    recovery: bool,
) -> Read<U256> {
    // Ordinary inspection never interprets the storage of a delegate of another capability.
    // Recovery explicitly installs this supported profile and must preserve its stored nonce.
    let profile = chain.railgun.as_ref().and_then(|railgun| {
        ExecutorProfile::accepted(chain.chain_id, railgun.deployment.relay_adapt_7702_contract)
    });
    match (profile, code) {
        (Some(_), Some(code))
            if code.is_empty() || recovery && executor_delegation(code).is_some() =>
        {
            answered(
                provider
                    .get_storage_at(address, EXECUTION_NONCE_STORAGE_SLOT)
                    .block_id(block_id)
                    .await,
            )
        }
        (Some(profile), Some(code)) if matches_executor_delegation(code, profile) => {
            call(provider, address, RelayAdapt7702::nonceCall {}, block_id).await
        }
        _ => (None, EndpointHealthOutcome::Healthy),
    }
}

/// Signing requires the confirmed execution nonce to remain current at the tip.
/// Account nonce and delegation come from the tip, not the older confirmed block.
pub(super) async fn inspect_for_signing(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<(ExecutorInspection, crate::vault::ExecutorNonceObservation)> {
    let (inspection, observed) = Box::pin(inspect_for_recovery_signing(
        chain, http, client, address, assets, true,
    ))
    .await?;
    Ok((
        inspection,
        observed.ok_or_else(|| eyre!("executor execution nonce is unknown"))?,
    ))
}

/// Public signing may originate from an EIP-7702 account with another delegate.
/// Its owner separately requires a known execution nonce when issued payloads exist.
pub(super) async fn inspect_for_recovery_signing(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
    require_execution_nonce: bool,
) -> Result<(
    ExecutorInspection,
    Option<crate::vault::ExecutorNonceObservation>,
)> {
    inspect_signing_state(
        chain,
        http,
        client,
        address,
        assets,
        require_execution_nonce,
        false,
    )
    .await
}

pub(super) async fn inspect_for_recovery_batch(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
) -> Result<(ExecutorInspection, crate::vault::ExecutorNonceObservation)> {
    let (inspection, observed) =
        inspect_signing_state(chain, http, client, address, assets, true, true).await?;
    Ok((
        inspection,
        observed.ok_or_else(|| eyre!("recovery execution nonce is unknown"))?,
    ))
}

async fn inspect_signing_state(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    address: Address,
    assets: &[ExecutorAsset],
    require_execution_nonce: bool,
    recovery: bool,
) -> Result<(
    ExecutorInspection,
    Option<crate::vault::ExecutorNonceObservation>,
)> {
    let endpoints =
        ObservationEndpoints::new(chain, http, crate::RpcTrafficClass::Sensitive, client)?;
    let mut attempts = endpoints.attempts();
    while let Some(endpoint) = attempts.next().await {
        let provider = &endpoint.provider;
        // The outcome of the inspection's requests, once they were answered or failed.
        let mut inspected = EndpointHealthOutcome::Healthy;
        // A rejection of the state the inspection answered reports `inspected` alone.
        let mut rejected = false;
        // `Err` moves on to the next endpoint. `Ok` is this endpoint's answer, a refusal included.
        let answer = async {
            // The head read carries no address. Address reads wait for the chain check.
            let head = trace_step("inspection_chain_head", async {
                let (chain_id, head) = join(provider.get_chain_id(), async {
                    provider.get_block_by_number(BlockNumberOrTag::Latest).await
                })
                .await;
                if chain_id? != chain.chain_id {
                    return Err(eyre!(WRONG_CHAIN));
                }
                Ok::<_, eyre::Report>(
                    head?
                        .ok_or_else(|| eyre!("latest executor block is unavailable"))?
                        .header
                        .num_hash(),
                )
            })
            .await?;
            let confirmed_number = head.number.saturating_sub(chain.finality_depth);
            let confirmed = trace_step("inspection_confirmed_block", async {
                provider.get_block_by_number(confirmed_number.into()).await
            })
            .await?
            .ok_or_else(|| eyre!("confirmed executor block is unavailable"))?
            .header
            .num_hash();
            let confirmed_nonce = async {
                if confirmed == head {
                    return (None, EndpointHealthOutcome::Healthy);
                }
                let (nonce, outcome) = if recovery {
                    // The reviewed target layout persists across delegation changes, including
                    // at the older block. Its runtime need not be installed at that block.
                    answered(
                        provider
                            .get_storage_at(address, EXECUTION_NONCE_STORAGE_SLOT)
                            .block_id(BlockId::hash_canonical(confirmed.hash))
                            .await,
                    )
                } else {
                    read_execution_nonce_at(provider, chain, address, confirmed).await
                };
                (Some(nonce), outcome)
            };
            // The confirmed nonce read starts after the tip account reads, so a stalled
            // tip code read never issues a second code read.
            let Ok((current, outcome, confirmed_nonce)) = trace_step("inspection_state", async {
                Ok::<_, Infallible>(
                    inspect_at_with(
                        provider,
                        chain,
                        address,
                        assets,
                        head,
                        recovery,
                        confirmed_nonce,
                    )
                    .await,
                )
            })
            .await;
            let (confirmed_nonce, confirmed_outcome) = confirmed_nonce;
            inspected = outcome.combine(confirmed_outcome);
            if current.account_nonce().is_none()
                || current.code().is_none()
                || current.balances().values().any(Option::is_none)
                || require_execution_nonce && current.execution_nonce().is_none()
            {
                rejected = true;
                return Err(eyre!("executor signing state is incomplete"));
            }
            let nonce = confirmed_nonce.unwrap_or_else(|| current.execution_nonce());
            if require_execution_nonce && nonce.is_none() {
                rejected = true;
                return Err(eyre!("confirmed executor execution nonce is unavailable"));
            }
            if current.execution_nonce() != nonce {
                return Ok(Err(eyre!(
                    "executor execution nonce is awaiting confirmation"
                )));
            }
            // After every state read, so a transaction sent meanwhile still blocks signing, and
            // before the head recheck, which must be the last read.
            let pending_nonce = provider.get_transaction_count(address).pending().await?;
            if current
                .account_nonce()
                .is_some_and(|nonce| nonce != pending_nonce)
            {
                return Ok(Err(eyre!(
                    "executor account has an unconfirmed transaction or authorization"
                )));
            }
            let canonical = trace_step("inspection_canonical_recheck", async {
                provider.get_block_by_number(head.number.into()).await
            })
            .await?
            .ok_or_else(|| eyre!("executor head block is unavailable"))?;
            if canonical.header.num_hash() != head {
                return Err(eyre!("executor head block changed during inspection"));
            }
            Ok::<_, eyre::Report>(Ok((
                current,
                nonce.map(|nonce| crate::vault::ExecutorNonceObservation::new(confirmed, nonce)),
            )))
        }
        .await;
        match answer {
            Ok(answer) => {
                endpoints.report(&endpoint, inspected);
                return answer;
            }
            Err(_) if rejected => endpoints.report(&endpoint, inspected),
            Err(error) => endpoints.failed_after(&endpoint, &error, inspected),
        }
    }
    Err(eyre!("executor signing state is unavailable"))
}

/// A return value that doesn't decode is `None`, yet the call was answered.
async fn call<C: SolCall>(
    provider: &DynProvider,
    to: Address,
    call: C,
    block: BlockId,
) -> Read<C::Return> {
    let (bytes, outcome) = answered(
        provider
            .call(
                TransactionRequest::default()
                    .to(to)
                    .input(call.abi_encode().into()),
            )
            .block(block)
            .await,
    );
    (
        bytes.and_then(|bytes| C::abi_decode_returns(&bytes).ok()),
        outcome,
    )
}

pub(super) fn executor_delegation(code: &[u8]) -> Option<Address> {
    // Alloy 2.3 exposes the designator but has no decoder for account code.
    Address::try_from(code.strip_prefix(&EIP7702_DELEGATION_DESIGNATOR)?).ok()
}

pub(crate) fn matches_executor_delegation(code: &[u8], profile: ExecutorProfile) -> bool {
    // Recognize EIP-7702 code designating any listed delegate of this profile's capability;
    // do not accept arbitrary code as delegation.
    executor_delegation(code).is_some_and(|delegate| profile.interchangeable(delegate).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{WalletSettings, build_effective_chain_configs};
    use alloy::primitives::B256;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use std::time::Duration;

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
        let client = OperationHttpClient::for_tests(
            rpc_client.clone(),
            crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Proxy),
        );
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
                &client,
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
    async fn a_failed_inspection_read_withdraws_its_endpoint_and_keeps_the_partial_result() {
        use serde_json::{Value, json};
        use std::sync::Arc;

        let mut chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 50;
        block.header.hash = B256::repeat_byte(50);
        // Passes the chain and head checks, then rate-limits the balance read.
        let (failing, failing_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| {
                let result = match request["method"].as_str().unwrap() {
                    "eth_chainId" => json!("0x1"),
                    "eth_getBlockByNumber" => serde_json::to_value(&block).unwrap(),
                    "eth_getTransactionCount" => json!("0x0"),
                    "eth_getCode" => json!("0x"),
                    "eth_getStorageAt" => json!(B256::ZERO),
                    _ => {
                        return json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "error": {"code": -32005, "message": "limit exceeded"}
                        });
                    }
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        // Another chain's endpoint, if tried first, fails the inspection's own chain check,
        // which is neutral and leaves it eligible.
        let (other, other_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(
                |request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x2"}),
            ),
            Arc::default(),
            Arc::default(),
        )
        .await;
        chain.rpc_route = crate::RpcChainRoute::new(1, vec![failing, other]);
        let client = OperationHttpClient::for_tests(
            reqwest::Client::new(),
            crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        );
        let http = HttpContext::direct_for_tests();
        let inspection = inspect_executor(
            &chain,
            &http,
            &client,
            Address::repeat_byte(1),
            &[ExecutorAsset::Native],
        )
        .await
        .unwrap();
        assert_eq!(inspection.account_nonce(), Some(0));
        assert_eq!(inspection.balances()[&ExecutorAsset::Native], None);

        // The strike reaches the broker on its advisory channel.
        let route = chain
            .rpc_route
            .for_class(crate::RpcTrafficClass::Sensitive)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while http
                .rpc_broker()
                .endpoint_candidates(route.route(), None)
                .await
                .unwrap()
                != route.endpoints()[1..]
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        failing_server.abort();
        other_server.abort();
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
        // An account on the earlier listed delegate keeps its nonce readable.
        let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
            .unwrap()
            .relay_adapt_7702_delegates[0]
            .address;
        for delegate in [profile.delegate(), earlier, Address::repeat_byte(2)] {
            let listed = delegate != Address::repeat_byte(2);
            responses.push_success(&"0x2");
            responses.push_success(&Bytes::from(
                [
                    EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                    delegate.as_slice(),
                ]
                .concat(),
            ));
            responses.push_success(&"0x0");
            if listed {
                responses.push_success(&Bytes::from(
                    RelayAdapt7702::nonceCall::abi_encode_returns(&U256::from(8)),
                ));
            }
            let inspection = inspect_at(&provider, &chain, address, &[], block, false).await;
            assert_eq!(inspection.activity(), ExecutorActivity::PreviouslyUsed);
            assert_eq!(inspection.account_nonce(), Some(2));
            assert_eq!(
                inspection.execution_nonce(),
                listed.then_some(U256::from(8))
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
