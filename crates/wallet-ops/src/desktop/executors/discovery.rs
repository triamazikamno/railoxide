use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse as _;
use alloy::providers::{Provider as _, bindings::IMulticall3};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};

use super::ExecutorOwner;
use crate::settings::ExecutorProfile;
use crate::vault::{ExecutorUseObservation, ProtectedSoftwareSeedSession, SpendGrant};
use crate::{DesktopPrivateSpendAuthorization, WalletSession};

/// Counts describe this request, not previous observations or transaction outcomes.
pub struct ExecutorDiscoveryReport {
    range: Range<u32>,
    used: usize,
    unused: usize,
    unavailable: usize,
}

impl ExecutorDiscoveryReport {
    #[must_use]
    pub fn range(&self) -> Range<u32> {
        self.range.clone()
    }
    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }
    #[must_use]
    pub const fn unused(&self) -> usize {
        self.unused
    }
    #[must_use]
    pub const fn unavailable(&self) -> usize {
        self.unavailable
    }
}

impl ExecutorOwner {
    pub async fn discover_authorized_range(
        &self,
        session: &WalletSession,
        authorization: &DesktopPrivateSpendAuthorization,
        range: Range<u32>,
    ) -> Result<ExecutorDiscoveryReport> {
        self.ensure_active()?;
        if session
            .executor_owner
            .as_ref()
            .is_none_or(|owner| !std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!(
                "account restoration belongs to a different wallet session"
            ));
        }
        let (mut grant, seed) = authorization.executor_spend_grant(&self.vault)?;
        self.discover_range(&mut grant, seed, range).await
    }

    /// Retain an explicitly authorized range, then check use in one aggregate.
    /// This never queries holdings or reconstructs unavailable operation history.
    pub async fn discover_range(
        &self,
        grant: &mut SpendGrant,
        protected_seed: Option<&ProtectedSoftwareSeedSession>,
        range: Range<u32>,
    ) -> Result<ExecutorDiscoveryReport> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        self.unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?
            .invalidate();
        let profile = ExecutorProfile::accepted(
            self.chain.chain_id,
            self.chain
                .require_railgun()?
                .deployment
                .relay_adapt_7702_contract,
        )
        .ok_or_else(|| eyre!("executor recovery profile is unavailable for this configuration"))?;
        let addresses = self.vault.executor_addresses_for_session(
            grant,
            &self.view,
            protected_seed,
            self.chain.chain_id,
            range.clone(),
        )?;
        // Stop and RPC failure must not discard already derived accounts.
        let records = addresses
            .iter()
            .map(|(index, address)| {
                self.store
                    .restore_index(*index, *address, profile.delegate(), &[])
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.notify_change();
        let observations = self
            .while_active(async { Ok(self.check_use(&addresses).await.ok()) })
            .await?;
        let mut report = ExecutorDiscoveryReport {
            range,
            used: 0,
            unused: 0,
            unavailable: 0,
        };
        for (index, record) in records.iter().enumerate() {
            self.ensure_active()?;
            let observation = observations.as_ref().and_then(|items| items[index]);
            match observation {
                Some(observation) if observation.was_used() => report.used += 1,
                Some(_) => report.unused += 1,
                None => report.unavailable += 1,
            }
            self.store
                .record_use_check(record.operation(), observation)?;
        }
        self.notify_change();
        Ok(report)
    }

    async fn check_use(
        &self,
        addresses: &[(u32, alloy::primitives::Address)],
    ) -> Result<Vec<Option<ExecutorUseObservation>>> {
        let multicall = self
            .chain
            .rpc_route
            .multicall()
            .ok_or_else(|| eyre!("Multicall is unavailable for this chain"))?;
        let call = IMulticall3::tryAggregateCall {
            requireSuccess: false,
            calls: addresses
                .iter()
                .map(|(_, address)| IMulticall3::Call {
                    target: *address,
                    callData: RelayAdapt7702::nonceCall {}.abi_encode().into(),
                })
                .collect(),
        };
        let pool = QueryRpcPool::with_http_client(
            self.chain.rpc_route.endpoint_urls(),
            Duration::from_secs(30),
            self.http.rpc_client.clone(),
        );
        for provider in pool.available_providers() {
            // Validate the chain before disclosing the derived range.
            if provider.provider.get_chain_id().await.ok() != Some(self.chain.chain_id) {
                continue;
            }
            let Ok(Some(head)) = provider
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await
            else {
                continue;
            };
            let block = head.header.num_hash();
            // Send one explicit aggregate, rather than broker members which can
            // fall back to individual calls. A failed aggregate ends this check.
            let output = provider
                .provider
                .call(
                    TransactionRequest::default()
                        .to(multicall)
                        .input(call.abi_encode().into()),
                )
                .block(BlockId::hash_canonical(block.hash))
                .await?;
            let results = IMulticall3::tryAggregateCall::abi_decode_returns_validate(&output)?;
            if results.len() != addresses.len() {
                return Err(eyre!("incomplete account use check"));
            }
            let checked_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            return Ok(results
                .into_iter()
                .map(|result| {
                    if !result.success {
                        return None;
                    }
                    // Low-level calls to undelegated EOAs succeed with empty bytes.
                    // A delegated getter returning zero is still evidence of use.
                    let nonce = if result.returnData.is_empty() {
                        None
                    } else {
                        Some(
                            RelayAdapt7702::nonceCall::abi_decode_returns_validate(
                                &result.returnData,
                            )
                            .ok()?,
                        )
                    };
                    Some(ExecutorUseObservation::new(block, checked_at, nonce))
                })
                .collect());
        }
        Err(eyre!("account use check is unavailable"))
    }
}
