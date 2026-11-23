use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, FixedBytes, TxKind, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use eyre::{Result, WrapErr, eyre};

use super::gas::resolve_public_action_gas_fee;
use super::signer::VaultedPublicSigner;
use super::types::{
    PublicActionAttemptInfo, PublicActionCommand, PublicActionCommandReceiver,
    PublicActionGasFeeQuote, PublicActionGasFeeSelection, PublicActionGasLimitStrategy,
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent, PublicActionSessionEventSender, PublicActionStepFeePolicy,
    PublicAssetId, PublicShieldTransactionProfile,
};
use crate::block_observer::BlockObserver;
use crate::desktop::ActivitySubmission;
use crate::settings::EffectiveChainGasSettings;
use crate::vault::ActivityStep;
use crate::{
    ClassRoute, HttpContext, OperationHttpClient, SelfBroadcastResolvedGasFee, TxReceiptOutput,
    report_chain_string, self_broadcast_replacement_bumped_fee,
    self_broadcast_send_raw_transaction_to_rpc_pool_with_logging,
};

mod attempt_broadcast;
mod gas_fee_authorization;
mod preflight_checks;
mod rpc_preflight;
mod step_events;
mod step_session;
mod transaction_requests;

pub(super) use attempt_broadcast::*;
pub(super) use gas_fee_authorization::*;
#[allow(clippy::wildcard_imports)]
use preflight_checks::*;
pub(super) use rpc_preflight::*;
pub use step_events::*;
pub(super) use step_session::*;
pub use transaction_requests::*;

/// Keeps the tracing target of the moved items at this module's path.
const LOG_TARGET: &str = module_path!();

pub(super) struct PublicActionStepOutcome {
    pub(super) receipt: TxReceiptOutput,
    pub(super) next_nonce: u64,
    pub(super) gas_fee: PublicActionGasFeeSelection,
}

/// A model-based funding check before estimation, plus fees reserved for a later Shield.
#[derive(Clone, Copy)]
pub(super) struct PublicShieldFundingBudget {
    pub(super) gas_units: u64,
    pub(super) remaining_shield_gas_units: u64,
    // Railway Auto still refreshes the later Shield under its captured ceiling after an
    // approval retry. Other flows carry the current approval fee into that Shield.
    pub(super) remaining_max_fee_per_gas: Option<u128>,
}

impl PublicShieldFundingBudget {
    fn remaining_reserve(self, max_fee_per_gas: u128) -> U256 {
        U256::from(self.remaining_shield_gas_units)
            * U256::from(self.remaining_max_fee_per_gas.unwrap_or(max_fee_per_gas))
    }
}

pub(super) struct PublicActionPreflight {
    tx_req: TransactionRequest,
    nonce: u64,
    gas_limit: u64,
    rpc_gas_price: u128,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
}

#[derive(Debug)]
pub(super) enum PublicActionPreflightError {
    FeeAuthorizationRequired {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        message: String,
    },
    InsufficientShieldFunding(eyre::Report),
    Other(eyre::Report),
}

impl From<eyre::Report> for PublicActionPreflightError {
    fn from(error: eyre::Report) -> Self {
        Self::Other(error)
    }
}

impl std::fmt::Display for PublicActionPreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeeAuthorizationRequired { message, .. } => formatter.write_str(message),
            Self::InsufficientShieldFunding(error) | Self::Other(error) => {
                std::fmt::Display::fmt(error, formatter)
            }
        }
    }
}

impl std::error::Error for PublicActionPreflightError {}

impl PublicActionPreflightError {
    pub(super) fn into_report(self) -> eyre::Report {
        match self {
            Self::FeeAuthorizationRequired { message, .. } => eyre!("{}", message),
            Self::InsufficientShieldFunding(error) | Self::Other(error) => error,
        }
    }
}

#[derive(Clone)]
pub(super) struct SubmittedPublicActionAttempt {
    pub(super) tx_hash: FixedBytes<32>,
    pub(super) info: PublicActionAttemptInfo,
    rpc_gas_price: u128,
}

/// Persists an attempt before its raw send: its hash, the signed request and the signed raw
/// transaction.
pub(super) type PublicActionHandoff<'a> =
    dyn FnMut(FixedBytes<32>, &TransactionRequest, &[u8]) -> Result<()> + Send + 'a;

/// Where a step session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicActionStepEnd {
    /// Once a receipt of a handed-off attempt is observed.
    Receipt,
    /// As [`Self::Receipt`], except that a first attempt that was handed off but that no RPC
    /// accepted ends the session without a receipt. The durable hand-off's owner follows that
    /// attempt from there. Only a session with a hand-off ends this way.
    HandOff,
    /// As [`Self::Receipt`], except that a successful receipt doesn't complete the step: the
    /// caller does once it verified the transaction's effect.
    Evidence,
}

/// What a step session ended with.
pub(super) enum PublicActionStepResult {
    Receipt(PublicActionStepOutcome),
    /// The first attempt was handed off and no RPC accepted it, as
    /// [`PublicActionStepEnd::HandOff`] allows.
    HandedOff,
}

impl PublicActionStepResult {
    /// The receipt a [`PublicActionStepEnd::Receipt`] session ends with.
    pub(super) fn into_receipt(self) -> Result<PublicActionStepOutcome> {
        match self {
            Self::Receipt(outcome) => Ok(outcome),
            Self::HandedOff => Err(eyre!(
                "the public action was handed off without waiting for its receipt"
            )),
        }
    }
}

/// The routes of a Public action, isolated over its operation client. Callers resolve them
/// before the transaction is signed, so a route without a Sensitive or Submit endpoint fails
/// before that signature.
pub(super) struct PublicActionPools {
    /// The Sensitive route preflights and estimates read through.
    pub(super) route: ClassRoute,
    /// A pool over `route`, for preflight fallbacks and block observation.
    pub(super) observe: Arc<QueryRpcPool>,
    /// The Submit endpoints a signed transaction fans out to.
    pub(super) submit: Arc<QueryRpcPool>,
}

pub(super) enum PublicActionAttemptError {
    Signing(eyre::Report),
    Sending(eyre::Report),
}

impl PublicActionAttemptError {
    pub(super) fn into_report(self) -> eyre::Report {
        match self {
            Self::Signing(error) | Self::Sending(error) => error,
        }
    }

    pub(super) fn message(&self) -> String {
        match self {
            Self::Signing(error) | Self::Sending(error) => report_chain_string(error),
        }
    }
}

#[cfg(test)]
mod tests;
