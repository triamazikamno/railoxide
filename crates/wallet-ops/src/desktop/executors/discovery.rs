use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse as _;
use alloy::primitives::Address;
use alloy::providers::Provider as _;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, eyre};
use rand::RngExt as _;
use rand::seq::SliceRandom as _;

use super::ExecutorOwner;
use crate::settings::ExecutorProfile;
use crate::vault::ExecutorUseObservation;
use crate::{DesktopPrivateSpendAuthorization, WalletSession};

/// A random 0.5-3 s wait before each address check after the first, so the endpoint can't
/// time the range's checks together.
fn check_jitter() -> Duration {
    let millis = if cfg!(test) { 1..=5 } else { 500..=3_000 };
    Duration::from_millis(rand::rng().random_range(millis))
}

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
        self.discover_range(authorization, range).await
    }

    /// Retain an explicitly authorized range, then check each address's use on its own route,
    /// in random order, recording each result as it arrives. Stopping leaves unattempted checks
    /// unrecorded. This never queries holdings or reconstructs unavailable operation history.
    pub async fn discover_range(
        &self,
        authorization: &DesktopPrivateSpendAuthorization,
        range: Range<u32>,
    ) -> Result<ExecutorDiscoveryReport> {
        self.ensure_active()?;
        let mut checks = self.retain_range(authorization, range.clone()).await?;
        checks.shuffle(&mut rand::rng());
        // Checks run without the activity lock, so a long restore doesn't block other work.
        self.while_active(async {
            let mut report = ExecutorDiscoveryReport {
                range,
                used: 0,
                unused: 0,
                unavailable: 0,
            };
            for (position, (address, operation)) in checks.into_iter().enumerate() {
                if position > 0 {
                    tokio::time::sleep(check_jitter()).await;
                }
                let observation = self.check_use(address).await.ok();
                {
                    let _guard = self.lock_activity().await;
                    // Shutdown closes before taking the lock, so no late result lands.
                    self.ensure_active()?;
                    self.store.record_use_check(operation, observation)?;
                    self.notify_change();
                }
                match observation {
                    Some(observation) if observation.was_used() => report.used += 1,
                    Some(_) => report.unused += 1,
                    None => report.unavailable += 1,
                }
            }
            Ok(report)
        })
        .await
    }

    /// Persist the range's derived identities before any address leaves the device.
    async fn retain_range(
        &self,
        authorization: &DesktopPrivateSpendAuthorization,
        range: Range<u32>,
    ) -> Result<Vec<(Address, crate::vault::ExecutorOperationId)>> {
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
        self.require_executor_source(
            authorization,
            &super::HardwareExecutorAction::Restore(range.clone()),
        )?;
        let addresses =
            if let DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
                hardware.restore(self, range.clone())?
            } else {
                let (mut grant, seed) = authorization.executor_spend_grant(&self.vault)?;
                self.vault.executor_addresses_for_session(
                    &mut grant,
                    &self.view,
                    seed,
                    self.chain.chain_id,
                    range.clone(),
                )?
            };
        // Stop and RPC failure must not discard already derived accounts.
        let records = addresses
            .iter()
            .map(|(index, address)| {
                self.store
                    .restore_index(*index, *address, profile.delegate(), &[])
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.notify_change();
        Ok(addresses
            .into_iter()
            .zip(records)
            .map(|((_, address), record)| (address, record.operation()))
            .collect())
    }

    /// Check one address on a fresh route of its own, never the shared one. Any failure
    /// leaves the address unavailable.
    async fn check_use(&self, address: Address) -> Result<ExecutorUseObservation> {
        let client = self.http.operation_http_client().await?;
        let pool = self
            .chain
            .rpc_route
            .for_class(crate::RpcTrafficClass::Sensitive)?
            .isolated(client)
            .query_rpc_pool(&self.http.rpc_clients(), Duration::from_secs(30));
        for provider in pool.available_providers() {
            // Validate the chain before disclosing the address.
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
            // A failed or reverted call ends this check.
            let output = provider
                .provider
                .call(
                    TransactionRequest::default()
                        .to(address)
                        .input(RelayAdapt7702::nonceCall {}.abi_encode().into()),
                )
                .block(BlockId::hash_canonical(block.hash))
                .await?;
            let checked_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            // Calls to undelegated EOAs succeed with empty bytes.
            // A delegated getter returning zero is still evidence of use.
            let nonce = if output.is_empty() {
                None
            } else {
                Some(RelayAdapt7702::nonceCall::abi_decode_returns_validate(
                    &output,
                )?)
            };
            return Ok(ExecutorUseObservation::new(block, checked_at, nonce));
        }
        Err(eyre!("account use check is unavailable"))
    }
}
