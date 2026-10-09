//! One tracking step of a swap paid from a Public account, chosen from its record.
//!
//! The record is on the destination chain's owner, so a swap that a restart left unfinished
//! resumes from there: [`ExecutorRecord::public_swaps_to_track`] lists it, and
//! [`ExecutorOwner::track_public_swap`] advances it. The step reads the chain the Public
//! account pays on through that chain's configuration, and its own chain through this owner.
//!
//! [`ExecutorRecord::public_swaps_to_track`]: crate::vault::ExecutorRecord::public_swaps_to_track

use alloy::eips::BlockNumHash;
use alloy::primitives::U256;
use eyre::{Result, eyre};

use super::public_order::RESUBMISSION_MARGIN;
use super::public_settlement::unix_now;
use crate::ExecutorOwner;
use crate::bridge::AcrossClient;
use crate::cow::CowOrderbookClient;
use crate::desktop::executor_observation::read_executor_asset_balance;
use crate::settings::EffectiveChainConfig;
use crate::vault::{ExecutorOperationId, SwapBridgeOutcome, SwapSubmissionStatus, SwapUseId};

/// What a tracking step of a swap paid from a Public account reads through.
pub struct PublicSwapTracking<'a> {
    /// The chain the Public account pays on.
    pub origin: &'a EffectiveChainConfig,
    pub across: &'a AcrossClient,
    /// Required for a swap on the order path.
    pub orderbook: Option<&'a CowOrderbookClient>,
    /// An explicit status check, which may replace a recorded outcome.
    pub explicit: bool,
}

/// What one tracking step found, for the caller to refresh what it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicSwapProgress {
    pub changed: bool,
    /// The Public account's balances on the origin chain changed: a refund or a withdrawal was
    /// verified.
    pub refresh_public_balances: bool,
    pub finished: bool,
    /// The destination token's confirmed balance, read only by an explicit check of a
    /// held-on-destination outcome.
    pub destination_balance: Option<(U256, BlockNumHash)>,
}

impl ExecutorOwner {
    /// Advance the swap of the use `swap_use` by what its record needs next, on this chain,
    /// the swap's destination.
    ///
    /// Until its deposit is handed off, an order's swap resends a signed order whose
    /// submission is still pending, unless it was stopped or can no longer be accepted in
    /// time, and then reads what became of the order. A direct deposit's swap reads its
    /// hand-off. With a hand-off the bridge is polled, as an explicit status check when
    /// `tracking.explicit`, and a refunding deposit's refund to the Public account is verified.
    /// A step with nothing to do is skipped, and the first step that fails returns its error.
    ///
    /// Nothing here takes a `WalletSession`, a signer or the origin chain's owner: every step
    /// reads the record on this owner and the origin chain through `tracking.origin`. A swap
    /// therefore resumes after a restart once its destination chain's owner is loaded, whether
    /// or not the chain it pays on has a private session.
    pub async fn track_public_swap(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        tracking: PublicSwapTracking<'_>,
    ) -> Result<PublicSwapProgress> {
        let PublicSwapTracking {
            origin,
            across,
            orderbook,
            explicit,
        } = tracking;
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let before = &claimed.swap;
        let now = unix_now()?;
        if before.observations().bridge_handoff.is_none() {
            if let Some(order) = before.order() {
                let orderbook = orderbook
                    .ok_or_else(|| eyre!("an order is tracked through its network's orderbook"))?;
                let awaits_submission = order.submission_status() == SwapSubmissionStatus::Pending
                    && claimed.live
                    && before.order_can_fill(now)
                    && u64::from(order.valid_to())
                        > now.saturating_add(RESUBMISSION_MARGIN.as_secs());
                if awaits_submission {
                    Box::pin(
                        self.resubmit_public_swap_order(operation, swap_use, origin, orderbook),
                    )
                    .await?;
                }
                Box::pin(self.observe_public_swap_order(operation, swap_use, origin, orderbook))
                    .await?;
            } else {
                Box::pin(self.observe_public_swap_handoff(operation, swap_use, origin)).await?;
            }
        }
        // `None` while there is no hand-off.
        let outcome = if explicit {
            Box::pin(self.check_public_swap_bridge(operation, swap_use, across)).await?
        } else {
            Box::pin(self.observe_public_swap_bridge(operation, swap_use, across)).await?
        };
        let refunded = if outcome == Some(SwapBridgeOutcome::Refunding) {
            Box::pin(self.observe_public_swap_refund(operation, swap_use, origin, across))
                .await?
                .is_some()
        } else {
            false
        };
        let destination_balance =
            if explicit && matches!(outcome, Some(SwapBridgeOutcome::HeldOnDestination { .. })) {
                let address = claimed
                    .destination
                    .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
                Some(
                    self.while_active(read_executor_asset_balance(
                        &self.endpoints,
                        &self.chain,
                        address,
                        crate::ExecutorAsset::Erc20(claimed.destination_token),
                    ))
                    .await?,
                )
            } else {
                None
            };

        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let (after_use, after) = record.public_swap_use(swap_use).ok_or_else(|| {
            eyre!("this stealth account isn't claimed by a swap paid from a Public account")
        })?;
        let withdrawn =
            before.observations().withdrawn.is_none() && after.observations().withdrawn.is_some();
        self.ensure_active()?;
        Ok(PublicSwapProgress {
            changed: after != before,
            refresh_public_balances: refunded || withdrawn,
            finished: after.is_finished(after_use.is_stopped(), now),
            destination_balance,
        })
    }
}
