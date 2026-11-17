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
mod tests;
