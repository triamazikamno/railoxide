//! Earlier Across approvals of a swap use that can still decide a new attempt's delivery.
//!
//! An earlier attempt's post-hook stays signed after its order expires, so it can run in a
//! later attempt's settlement and deposit under its own approved destination minimum.
//!
//! Nothing here reads an order's `valid_to`, its expiry or death observations, any clock, or a
//! local constant such as the 3600-second quote coverage. These review exclusions never touch
//! recorded hand-offs or their tracking.

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider as _};
use broadcaster_core::contracts::cow::OrderUid;
use eyre::{Result, eyre};
use tracing::Instrument as _;

use super::observation::call_at;
use crate::desktop::executor_discovery::execution_nonce_at;
use crate::desktop::executor_observation::trace_step;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    BridgeOrderTerms, BridgeProvider, ExecutorOperationId, ExecutorRecord, SwapDelivery, SwapUseId,
};
use crate::{ExecutorOwner, OperationHttpClient};

// The pinned shared `SpokePool` binding declares `depositV3` and the deposit and fill events,
// and no getter for the quote-age limit. This matches the deployed contracts.
alloy::sol! {
    interface QuoteAgeSpokePool {
        function depositQuoteTimeBuffer() external view returns (uint32);
    }
}

/// What one finalized canonical block of the swap's chain shows for the quote-age rule. All
/// four facts come from the same block; a wall clock, a newer header or a local constant can't
/// stand in for any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapQuoteAgeEvidence {
    pub block: BlockNumHash,
    /// The block's timestamp.
    pub timestamp: u64,
    /// The account's execution nonce at the block.
    pub nonce: U256,
    /// `depositQuoteTimeBuffer()` of the pinned `SpokePool` at the block.
    pub quote_time_buffer: u32,
}

/// An earlier attempt's Across approval that can still determine a new attempt's delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapContinuingApproval {
    pub uid: OrderUid,
    pub attempt: u32,
    /// The approval's signed output amount on the destination chain.
    pub destination_minimum: U256,
    pub post_hook_nonce: U256,
    pub quote_timestamp: u32,
}

/// The continuing approvals of one swap use for a newly reviewed attempt, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwapContinuingApprovals {
    approvals: Vec<SwapContinuingApproval>,
}

impl SwapContinuingApprovals {
    /// Continuing approvals as given, for a review that read none.
    #[cfg(any(feature = "test-support", debug_assertions))]
    pub(super) const fn for_tests(approvals: Vec<SwapContinuingApproval>) -> Self {
        Self { approvals }
    }
    #[must_use]
    pub fn approvals(&self) -> &[SwapContinuingApproval] {
        &self.approvals
    }
    /// The lowest continuing destination minimum below `minimum`, the new attempt's own. `None`
    /// when no continuing approval can deliver less.
    #[must_use]
    pub fn lower_minimum(&self, minimum: U256) -> Option<U256> {
        self.approvals
            .iter()
            .map(|approval| approval.destination_minimum)
            .filter(|continuing| *continuing < minimum)
            .min()
    }
}

/// Whether `delivery` is an Across delivery to a public receiver, the only kind whose earlier
/// approvals continue.
pub(super) fn is_public_across(delivery: SwapDelivery) -> bool {
    matches!(
        delivery,
        SwapDelivery::Bridge(bridge)
            if bridge.provider == BridgeProvider::Across && !bridge.is_private()
    )
}

/// The approvals of earlier attempts of `swap_use` that can still deliver for an attempt with
/// `delivery` whose pre-hook is signed at `pre_hook_nonce`. An approval whose post-hook nonce
/// is at or below that nonce is excluded, since the new pre-hook consumes or passes it. With
/// `evidence`, an approval is also excluded when its post-hook nonce is still unused at the
/// evidence block and that block is past the signed quote timestamp plus the pool's limit. An
/// unknown nonce and missing evidence exclude nothing. Pure, without I/O.
#[must_use]
pub fn swap_continuing_approvals(
    record: &ExecutorRecord,
    swap_use: SwapUseId,
    delivery: SwapDelivery,
    pre_hook_nonce: Option<U256>,
    evidence: Option<&SwapQuoteAgeEvidence>,
) -> SwapContinuingApprovals {
    if !is_public_across(delivery) {
        return SwapContinuingApprovals::default();
    }
    let approvals = record
        .swap_use_orders(swap_use)
        .filter(|order| order.delivery() == delivery)
        .filter_map(|order| {
            let post_hook_nonce = order.post_hook()?.nonce();
            let Some(BridgeOrderTerms::Across(terms)) = order.bridge() else {
                return None;
            };
            if pre_hook_nonce.is_some_and(|nonce| post_hook_nonce <= nonce) {
                return None;
            }
            // Equality at the age boundary still permits a deposit.
            if evidence.is_some_and(|evidence| {
                evidence.nonce < post_hook_nonce
                    && evidence.timestamp
                        > u64::from(terms.quote_timestamp) + u64::from(evidence.quote_time_buffer)
            }) {
                return None;
            }
            Some(SwapContinuingApproval {
                uid: order.uid(),
                attempt: order.attempt(),
                destination_minimum: terms.output_amount,
                post_hook_nonce,
                quote_timestamp: terms.quote_timestamp,
            })
        })
        .collect();
    SwapContinuingApprovals { approvals }
}

impl ExecutorOwner {
    /// The continuing approvals of `swap_use` for an attempt with `delivery` whose pre-hook is
    /// signed at `pre_hook_nonce`, read for review and again for signing. Nothing is requested
    /// when the nonce comparison alone leaves none. Otherwise the quote-age evidence is read
    /// from one finalized block over `client`; when it can't be read, every remaining approval
    /// is kept.
    pub async fn swap_continuing_approvals(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        delivery: SwapDelivery,
        pre_hook_nonce: Option<U256>,
        client: &OperationHttpClient,
    ) -> Result<SwapContinuingApprovals> {
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let retained = swap_continuing_approvals(&record, swap_use, delivery, pre_hook_nonce, None);
        if retained.approvals().is_empty() {
            return Ok(retained);
        }
        Ok(match self.swap_quote_age_evidence(&record, client).await {
            Some(evidence) => swap_continuing_approvals(
                &record,
                swap_use,
                delivery,
                pre_hook_nonce,
                Some(&evidence),
            ),
            None => retained,
        })
    }

    /// `None` whenever the evidence can't be read, whatever the reason.
    async fn swap_quote_age_evidence(
        &self,
        record: &ExecutorRecord,
        client: &OperationHttpClient,
    ) -> Option<SwapQuoteAgeEvidence> {
        // Only this chain's own SpokePool takes the deposits of its orders.
        let spoke_pool = self
            .chain
            .bridge_profile()
            .map(|profile| profile.spoke_pool())?;
        let executor = record.address()?;
        let mut chain = self.chain_for_delegate(record.delegate())?;
        chain.enabled = true;
        let endpoints = self.observation_endpoints(client).ok()?;
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_quote_age",
                self.while_active(Box::pin(read_quote_age_evidence(
                    &endpoint.provider,
                    &chain,
                    executor,
                    spoke_pool,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(evidence) => {
                    endpoints.succeeded(&endpoint);
                    return evidence;
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        None
    }
}

/// The block identity and timestamp of block `number`.
async fn quote_age_header(provider: &DynProvider, number: u64) -> Result<(BlockNumHash, u64)> {
    let block = trace_step("quote_age_block", async {
        provider.get_block_by_number(number.into()).await
    })
    .await?
    .ok_or_else(|| eyre!("quote-age block is unavailable"))?;
    if block.header.number != number {
        return Err(eyre!("quote-age block does not match its height"));
    }
    Ok((
        BlockNumHash::new(number, block.header.hash),
        block.header.timestamp,
    ))
}

/// Every fact is read at the same finalized block, which must still be canonical afterwards.
/// `Ok(None)` when the chain has no finalized block yet, the nonce is unreadable, or the block
/// changed during the read.
async fn read_quote_age_evidence(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    executor: Address,
    spoke_pool: Address,
) -> Result<Option<SwapQuoteAgeEvidence>> {
    let head = trace_step("quote_age_head", provider.get_block_number()).await?;
    let Some(number) = head.checked_sub(chain.finality_depth) else {
        return Ok(None);
    };
    let (block, timestamp) = quote_age_header(provider, number).await?;
    let Some(nonce) = execution_nonce_at(provider, chain, executor, block).await else {
        return Ok(None);
    };
    let quote_time_buffer = call_at(
        provider,
        spoke_pool,
        QuoteAgeSpokePool::depositQuoteTimeBufferCall {},
        BlockId::hash_canonical(block.hash),
    )
    .await?;
    if quote_age_header(provider, number).await?.0 != block {
        return Ok(None);
    }
    Ok(Some(SwapQuoteAgeEvidence {
        block,
        timestamp,
        nonce,
        quote_time_buffer,
    }))
}
