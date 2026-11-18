use super::*;
use async_trait::async_trait;
use eyre::eyre;

use crate::block_observer::BlockObserver;
use crate::public_wallet::PublicTransactionObservationGuard;
use crate::rpc_broker::{EndpointHealthOutcome, RpcBrokerError, RpcCandidates};
use std::collections::HashSet;

mod fee_quote;
mod preflight;
mod raw_tx;
mod sponsored_session;
mod submission;

pub use fee_quote::*;
pub(crate) use preflight::*;
pub(crate) use raw_tx::*;
pub use sponsored_session::*;
pub(crate) use submission::*;

/// Keeps the tracing target of the moved functions at this module's path.
const LOG_TARGET: &str = module_path!();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSponsoredSelfBroadcastResult {
    pub prepared: PreparedSponsoredCall,
    pub outcome: SponsoredSelfBroadcastSessionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfBroadcastTxOutcome {
    Receipt(TxReceiptOutput),
    /// The receipt of a transaction the wallet sent for a stealth account, with whether
    /// the account's nonce was read past the payload's afterwards.
    ExecutorReceipt {
        receipt: TxReceiptOutput,
        state: vault::ExecutorPayloadState,
    },
    InclusionUnobserved {
        tx_hash: String,
    },
}

impl SelfBroadcastTxOutcome {
    #[must_use]
    pub const fn receipt(&self) -> Option<&TxReceiptOutput> {
        match self {
            Self::Receipt(receipt) | Self::ExecutorReceipt { receipt, .. } => Some(receipt),
            Self::InclusionUnobserved { .. } => None,
        }
    }

    /// A stealth account's receipt tells a revert at the time. Its success establishes
    /// completion only once the account's nonce is read past the payload's.
    #[must_use]
    pub const fn execution_status(&self) -> Option<bool> {
        match self {
            Self::Receipt(receipt) => Some(receipt.status),
            Self::ExecutorReceipt { receipt, state } => match (receipt.status, state) {
                (false, _) => Some(false),
                (true, vault::ExecutorPayloadState::Resolved) => Some(true),
                (true, vault::ExecutorPayloadState::Pending) => None,
            },
            Self::InclusionUnobserved { .. } => None,
        }
    }

    #[must_use]
    pub fn tx_hash(&self) -> &str {
        match self {
            Self::Receipt(receipt) | Self::ExecutorReceipt { receipt, .. } => &receipt.tx_hash,
            Self::InclusionUnobserved { tx_hash } => tx_hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSelfBroadcastResult {
    pub chain_id: u64,
    pub public_account_uuid: String,
    pub gas_payer: Address,
    pub gas_limit: u64,
    pub rpc_gas_price: u128,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub estimated_native_gas_cost: U256,
    pub live_native_balance: U256,
    pub tx: SelfBroadcastTxOutcome,
    pub attempts: Vec<SelfBroadcastAttemptInfo>,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

pub(super) struct SelfBroadcastPreflight {
    pub(super) tx_req: TransactionRequest,
    pub(super) nonce: u64,
    pub(super) gas_limit: u64,
    pub(super) rpc_gas_price: u128,
    pub(super) max_fee_per_gas: u128,
    pub(super) max_priority_fee_per_gas: u128,
    pub(super) estimated_native_gas_cost: U256,
    pub(super) live_native_balance: U256,
}

pub(super) struct SubmittedSelfBroadcastAttempt {
    pub(super) info: SelfBroadcastAttemptInfo,
    pub(super) rpc_gas_price: u128,
    pub(super) estimated_native_gas_cost: U256,
    pub(super) live_native_balance: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelfBroadcastWinnerOutput {
    gas_limit: u64,
    rpc_gas_price: u128,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    estimated_native_gas_cost: U256,
    live_native_balance: U256,
}

const fn self_broadcast_winner_output(
    attempts: &[SubmittedSelfBroadcastAttempt],
    winner_index: usize,
) -> SelfBroadcastWinnerOutput {
    let winner = &attempts[winner_index];
    SelfBroadcastWinnerOutput {
        gas_limit: winner.info.gas_limit,
        rpc_gas_price: winner.rpc_gas_price,
        max_fee_per_gas: winner.info.max_fee_per_gas,
        max_priority_fee_per_gas: winner.info.max_priority_fee_per_gas,
        estimated_native_gas_cost: winner.estimated_native_gas_cost,
        live_native_balance: winner.live_native_balance,
    }
}

fn self_broadcast_observation_outcome(
    observation: Result<crate::block_observer::BlockObservation>,
    attempts: &[SubmittedSelfBroadcastAttempt],
) -> Result<Option<(usize, SelfBroadcastTxOutcome)>> {
    match observation {
        Ok(observation) => Ok(observation
            .receipt
            .map(|(index, receipt)| (index, SelfBroadcastTxOutcome::Receipt(receipt)))),
        Err(error) => {
            let index = attempts.len().checked_sub(1).ok_or(error)?;
            Ok(Some((
                index,
                SelfBroadcastTxOutcome::InclusionUnobserved {
                    tx_hash: attempts[index].info.tx_hash.clone(),
                },
            )))
        }
    }
}

const TREZOR_APP_PASSPHRASE_ERROR_TEXT: &str =
    "Trezor requested an app-entered passphrase but none was provided";
const TREZOR_SELF_BROADCAST_RESTART_GUIDANCE: &str = "Trezor app passphrase is required again. Re-unlock the hardware profile with the app passphrase, then restart this self-broadcast.";
const NATIVE_TOP_UP_SELF_BROADCAST_GAS_GUIDANCE: &str = "Native gas generated by this unshield is delivered after the self-broadcast transaction executes and cannot pay for the current outer transaction.";

#[cfg(test)]
mod tests;
