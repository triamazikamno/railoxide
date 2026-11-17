//! Tron Send as a desktop-owned draft. GPUI renders it and passes
//! the user's decisions:
//!
//! 1. [`parse_tron_receiver`] checks the receiver field, and [`tron_receiver_suggestions`] fills
//!    its suggestions.
//! 2. [`prepare_tron_send`] runs on each estimate revision and returns the [`TronSendReview`];
//!    Review is available when it has no `missing_trx`.
//! 3. After the user approves that review, [`submit_tron_send`] awaits the signer future, which
//!    performs authorization and any hardware approval. It re-reads fees, and returns a new review
//!    instead of signing if the maximum rose. Otherwise it builds the transaction on a fresh
//!    reference block, signs it, hands it to an endpoint and follows it to its outcome with
//!    [`super::confirmation::confirm_tron_send`].
//!
//! Stop is dropping that future before it emits [`PublicActionSessionEvent::AttemptHandoff`], as
//! for EVM Public Send: nothing is persisted or broadcast before then. Afterwards the outcome
//! comes only from solidified blocks, whatever the endpoint answered. The caller's `claim_handoff`
//! decides atomically between a Stop and the handoff.

use super::address::parse_tron_address;
use super::confirmation::{
    TronBlockReads, TronReads, TronSendHandoff, TronSendOutcome, confirm_tron_send_at,
};
use super::fees::{
    AccountResources, ChainFeeParameters, FeeInputs, TronFees, bandwidth_bytes, fee_limit_sun,
    max_trx_amount, missing_trx, tron_fees,
};
use super::http_api::TronBroadcast;
use super::transaction::{
    ReferenceBlock, SignedTransaction, TronAsset, TronTransfer, UnsignedTransaction,
};
use crate::bridge::NonEvmNetwork;
use crate::desktop::ActivitySubmission;
use crate::public_wallet::PublicErc20;
use crate::vault::{
    ActivityAsset, ActivityAttemptOutcome, ActivityDirection, ActivityLeg, DesktopVaultStore,
    DesktopViewSession, NonEvmAddressBookEntry, TronAccountMetadata,
};
use crate::{
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent, PublicActionSessionEventSender, RpcBrokerError, RpcRead,
};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256, Bytes, U64, U256};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::serde::WithOtherFields;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall as _;
use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum TronSendError {
    /// An EVM address, a Tron hex address or another network's address.
    #[error("a Tron address is needed")]
    NotATronAddress,
    /// The sender doesn't exist on chain, for example after receiving only tokens.
    #[error("the account must first receive TRX")]
    SenderMissing,
    /// Tron refuses native transfers to the sending account.
    #[error("TRX can't be sent to the sending account")]
    SelfTransfer,
    #[error("the amount must be more than zero")]
    ZeroAmount,
    /// The transaction is built right before signing, so this shouldn't happen.
    #[error("the transaction expired before it was handed to an endpoint")]
    Expired,
    /// The caller's handoff claim lost to a Stop.
    #[error("the send was stopped before it was handed to an endpoint")]
    Stopped,
    #[error(transparent)]
    Read(#[from] RpcBrokerError),
    #[error("{0:#}")]
    Other(eyre::Report),
}

// `eyre::Report` isn't a `std::error::Error`, so thiserror's `#[from]` can't take it.
impl From<eyre::Report> for TronSendError {
    fn from(error: eyre::Report) -> Self {
        Self::Other(error)
    }
}

/// The receiver field's text as a mainnet base58 Tron address.
pub fn parse_tron_receiver(text: &str) -> Result<Address, TronSendError> {
    parse_tron_address(text.trim()).map_err(|_| TronSendError::NotATronAddress)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TronReceiverSuggestion {
    pub label: Option<String>,
    pub address: Address,
}

/// The address book's Tron entries, then the wallet's visible Tron accounts other than `sender`.
#[must_use]
pub fn tron_receiver_suggestions(
    address_book: &[NonEvmAddressBookEntry],
    visible_accounts: &[TronAccountMetadata],
    sender: Address,
) -> Vec<TronReceiverSuggestion> {
    let book = address_book
        .iter()
        .filter(|entry| entry.network == NonEvmNetwork::Tron)
        .filter_map(|entry| {
            Some(TronReceiverSuggestion {
                label: Some(entry.label.clone()),
                address: parse_tron_address(&entry.address).ok()?,
            })
        });
    let own = visible_accounts
        .iter()
        .filter(|account| account.address != sender)
        .map(|account| TronReceiverSuggestion {
            label: account.label.clone(),
            address: account.address,
        });
    book.chain(own).collect()
}

/// The reads and broadcast a Send makes, and the clock it checks expiration against.
#[async_trait::async_trait]
pub trait TronSendReads: Send + Sync {
    async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError>;
    async fn account_resources(&self, account: Address)
    -> Result<AccountResources, RpcBrokerError>;
    async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError>;
    /// `eth_estimateGas` for `sender` calling `contract` with `data`.
    async fn energy_estimate(
        &self,
        sender: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<u64, RpcBrokerError>;
    /// The latest block, which a transaction about to be signed references.
    async fn latest_block(&self) -> Result<ReferenceBlock, RpcBrokerError>;
    async fn broadcast(
        &self,
        transaction: &SignedTransaction,
    ) -> Result<TronBroadcast, RpcBrokerError>;
    /// Unix time in milliseconds.
    fn now_ms(&self) -> u64;
}

#[async_trait::async_trait]
impl TronSendReads for TronReads {
    async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError> {
        self.api.account_exists(account).await
    }

    async fn account_resources(
        &self,
        account: Address,
    ) -> Result<AccountResources, RpcBrokerError> {
        // A Send has already refused a sender not on chain, and a missing account has none.
        Ok(self
            .api
            .account_resources(account)
            .await?
            .unwrap_or_default())
    }

    async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError> {
        self.api.chain_fee_parameters().await
    }

    async fn energy_estimate(
        &self,
        sender: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<u64, RpcBrokerError> {
        let request = TransactionRequest {
            from: Some(sender),
            to: Some(contract.into()),
            input: TransactionInput::new(data),
            ..TransactionRequest::default()
        };
        // Tron refuses a block parameter here.
        let read = RpcRead::estimate_gas(WithOtherFields::new(request), None, super::TRON_CHAIN_ID)
            .map_err(RpcBrokerError::InvalidRead)?;
        let energy: U64 = serde_json::from_value(self.json_rpc(read).await?)
            .map_err(|_| RpcBrokerError::InvalidResponse)?;
        Ok(energy.to())
    }

    async fn latest_block(&self) -> Result<ReferenceBlock, RpcBrokerError> {
        let block = self.block_at(BlockNumberOrTag::Latest).await?;
        Ok(ReferenceBlock {
            number: block.number,
            hash: block.hash,
            timestamp_ms: block.timestamp_ms,
        })
    }

    async fn broadcast(
        &self,
        transaction: &SignedTransaction,
    ) -> Result<TronBroadcast, RpcBrokerError> {
        self.api.broadcast(transaction).await
    }

    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TronSendAsset {
    Trx,
    Trc20(Address),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TronSendAmount {
    /// In sun for TRX, in the token's units otherwise.
    Exact(U256),
    /// TRX only: the balance less the maximum fee.
    Max,
}

/// The Send form's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TronSendDraft {
    pub sender: Address,
    pub receiver: Address,
    pub asset: TronSendAsset,
    pub amount: TronSendAmount,
    /// The sender's TRX balance in sun, from its latest refresh.
    pub trx_balance: u64,
}

/// One estimate revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TronSendReview {
    /// What signing is bound to: sender, receiver, asset, amount and, for a token, the fee limit,
    /// which is the reviewed energy cap at the reviewed energy price. Not the reference block.
    pub transfer: TronTransfer,
    pub fees: TronFees,
    /// The fee parameters the maximum is computed from.
    pub parameters: ChainFeeParameters,
    /// A TRX send that creates the receiver's account.
    pub creates_account: bool,
    /// The sender's resources the fees are computed from.
    pub resources: AccountResources,
    /// The bandwidth the transaction is charged for.
    pub bandwidth_bytes: u64,
    /// A token send whose energy estimate is more than 25% above the same transfer back to the
    /// sender, as when the receiver doesn't hold the token yet.
    pub receiver_lacks_token: bool,
    pub trx_balance: u64,
    /// The TRX Review lacks for the amount plus the expected fee. Review is available only when
    /// this is `None`.
    pub missing_trx: Option<u64>,
}

/// Prepares one estimate revision of `draft`.
pub async fn prepare_tron_send<R: TronSendReads + ?Sized>(
    reads: &R,
    draft: &TronSendDraft,
) -> Result<TronSendReview, TronSendError> {
    if draft.asset == TronSendAsset::Trx && draft.receiver == draft.sender {
        return Err(TronSendError::SelfTransfer);
    }
    if !reads.account_exists(draft.sender).await? {
        return Err(TronSendError::SenderMissing);
    }
    let amount = match (draft.amount, draft.asset) {
        (TronSendAmount::Exact(amount), _) => amount,
        // The balance's fees bound a smaller amount's, whose encoding is no longer.
        (TronSendAmount::Max, TronSendAsset::Trx) => U256::from(draft.trx_balance),
        (TronSendAmount::Max, TronSendAsset::Trc20(_)) => {
            return Err(eyre::eyre!("Max applies to TRX").into());
        }
    };
    let basis = read_fee_basis(reads, draft.sender, draft.receiver, draft.asset, amount).await?;
    let now_ms = reads.now_ms();
    let review = build_review(draft, amount, &basis, now_ms)?;
    if draft.amount != TronSendAmount::Max {
        return Ok(review);
    }
    let amount = max_trx_amount(draft.trx_balance, review.fees.maximum);
    build_review(draft, U256::from(amount), &basis, now_ms)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TronSendSubmission {
    /// The fees rose above the review, so nothing was signed. The user reviews this instead.
    Rereview(TronSendReview),
    /// Handed to an endpoint and decided from solidified blocks.
    Decided {
        handoff: TronSendHandoff,
        outcome: TronSendOutcome,
    },
}

/// The submitting wallet's session, which journals the send under its Tron account.
pub struct TronSendActivity {
    pub vault_store: Arc<DesktopVaultStore>,
    pub view_session: Arc<DesktopViewSession>,
    pub tron_account_uuid: String,
}

/// Signs `review` once `signer` resolves, hands it to an endpoint and follows it to its outcome.
/// `signer` performs authorization and any hardware approval; the transaction is built after
/// it, on the latest block. Progress and session events follow EVM Public Send's, all on the
/// Send step. With `activity` the send is journaled at its handoff, and its outcome once
/// decided.
pub async fn submit_tron_send<R: TronSendReads + TronBlockReads + ?Sized>(
    reads: &R,
    review: &TronSendReview,
    signer: impl Future<Output = eyre::Result<PrivateKeySigner>> + Send,
    event_tx: Option<&PublicActionSessionEventSender>,
    activity: Option<TronSendActivity>,
    claim_handoff: impl FnOnce() -> bool + Send,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<TronSendSubmission, TronSendError> {
    progress(send_progress(
        PublicActionProgressStatus::Pending,
        None,
        None,
    ));
    let signer = signer.await?;
    let transfer = review.transfer;
    let (asset, amount) = match transfer.asset {
        TronAsset::Trx { amount_sun } => (TronSendAsset::Trx, U256::from(amount_sun)),
        TronAsset::Trc20 {
            contract, amount, ..
        } => (TronSendAsset::Trc20(contract), amount),
    };
    let basis = read_fee_basis(reads, transfer.sender, transfer.receiver, asset, amount).await?;
    let now_ms = reads.now_ms();
    let draft = TronSendDraft {
        sender: transfer.sender,
        receiver: transfer.receiver,
        asset,
        amount: TronSendAmount::Exact(amount),
        trx_balance: review.trx_balance,
    };
    let current = build_review(&draft, amount, &basis, now_ms)?;
    // The reviewed fee limit must still cover the energy, and the reviewed transaction must not
    // be able to burn more than reviewed.
    let reviewed_fee_limit = fee_limit(&transfer);
    let (signed_fees, _) = transfer_fees(&transfer, &basis, now_ms)?;
    let signed_maximum = signed_fees
        .maximum
        .saturating_sub(signed_fees.fee_limit)
        .saturating_add(reviewed_fee_limit);
    if current.fees.fee_limit > reviewed_fee_limit || signed_maximum > review.fees.maximum {
        return Ok(TronSendSubmission::Rereview(current));
    }

    let block = reads.latest_block().await?;
    let signed = UnsignedTransaction::build(&transfer, &block, reads.now_ms())?.sign(&signer)?;
    drop(signer);
    if reads.now_ms() >= signed.expiration_ms() {
        return Err(TronSendError::Expired);
    }
    if !claim_handoff() {
        return Err(TronSendError::Stopped);
    }
    let activity = activity.map(|activity| tron_send_activity(activity, &transfer));
    if let Some(activity) = &activity {
        activity.add_attempt(signed.txid());
    }
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(PublicActionSessionEvent::AttemptHandoff {
            step: PublicActionProgressStep::Send,
        });
    }
    let txid = alloy::hex::encode(signed.txid());
    progress(send_progress(
        PublicActionProgressStatus::Pending,
        Some(txid.clone()),
        None,
    ));
    // No answer ends the send: inclusion stays unknown until solidified blocks decide it.
    let endpoint_reason = match reads.broadcast(&signed).await {
        Ok(TronBroadcast { accepted: true, .. }) => None,
        Ok(TronBroadcast { code, .. }) => Some(format!(
            "the endpoint refused the transaction ({})",
            code.as_deref().unwrap_or("no code")
        )),
        Err(error) => Some(format!("the endpoint didn't answer the broadcast: {error}")),
    };
    progress(send_progress(
        PublicActionProgressStatus::Pending,
        Some(txid.clone()),
        endpoint_reason,
    ));

    let handoff = TronSendHandoff {
        transfer,
        txid: signed.txid(),
        reference_block: block.number,
        expiration_ms: signed.expiration_ms(),
    };
    let (outcome, decided_ms) = confirm_tron_send_at(reads, &handoff).await;
    if let Some(activity) = &activity {
        activity.record_outcome(handoff.txid, tron_attempt_outcome(outcome, decided_ms));
    }
    let (status, message) = match outcome {
        TronSendOutcome::Complete { .. } => (PublicActionProgressStatus::Done, None),
        TronSendOutcome::Failed {
            reason,
            trx_burned_sun,
            ..
        } => (
            PublicActionProgressStatus::Error,
            Some(format!(
                "{reason}; it burned {}",
                railgun_ui::tron_native_currency().format_amount(U256::from(trx_burned_sun))
            )),
        ),
        TronSendOutcome::Dropped => (
            PublicActionProgressStatus::Error,
            Some("the transaction expired without being included; no TRX was spent".to_owned()),
        ),
    };
    progress(send_progress(status, Some(txid), message));
    Ok(TronSendSubmission::Decided { handoff, outcome })
}

fn tron_send_activity(activity: TronSendActivity, transfer: &TronTransfer) -> ActivitySubmission {
    let (asset, amount) = match transfer.asset {
        TronAsset::Trx { amount_sun } => (ActivityAsset::Native, U256::from(amount_sun)),
        TronAsset::Trc20 {
            contract, amount, ..
        } => (ActivityAsset::Token(contract), amount),
    };
    ActivitySubmission::tron(
        &activity.vault_store,
        &activity.view_session,
        activity.tron_account_uuid,
        ActivityLeg {
            asset,
            amount,
            direction: ActivityDirection::Out,
        },
        transfer.receiver,
    )
}

/// The journaled outcome of a send decided by a block at `decided_ms`.
pub(crate) const fn tron_attempt_outcome(
    outcome: TronSendOutcome,
    decided_ms: u64,
) -> ActivityAttemptOutcome {
    let timestamp = decided_ms / 1_000;
    match outcome {
        TronSendOutcome::Complete { block, .. } => ActivityAttemptOutcome::Included {
            block,
            timestamp,
            success: true,
        },
        TronSendOutcome::Failed { block, .. } => ActivityAttemptOutcome::Included {
            block,
            timestamp,
            success: false,
        },
        TronSendOutcome::Dropped => ActivityAttemptOutcome::Dropped,
    }
}

const fn send_progress(
    status: PublicActionProgressStatus,
    tx_hash: Option<String>,
    message: Option<String>,
) -> PublicActionProgressUpdate {
    PublicActionProgressUpdate {
        step: PublicActionProgressStep::Send,
        status,
        tx_hash,
        message,
    }
}

/// What the fees are computed from.
struct FeeBasis {
    parameters: ChainFeeParameters,
    resources: AccountResources,
    energy_estimate: u64,
    creates_account: bool,
    receiver_lacks_token: bool,
}

async fn read_fee_basis<R: TronSendReads + ?Sized>(
    reads: &R,
    sender: Address,
    receiver: Address,
    asset: TronSendAsset,
    amount: U256,
) -> Result<FeeBasis, RpcBrokerError> {
    let parameters = reads.chain_fee_parameters().await?;
    let resources = reads.account_resources(sender).await?;
    let (energy_estimate, creates_account, receiver_lacks_token) = match asset {
        TronSendAsset::Trx => (0, !reads.account_exists(receiver).await?, false),
        TronSendAsset::Trc20(contract) => {
            let transfer = |recipient: Address, amount: U256| {
                Bytes::from(PublicErc20::transferCall { recipient, amount }.abi_encode())
            };
            let estimate = reads
                .energy_estimate(sender, contract, transfer(receiver, amount))
                .await?;
            // The sender keeps a nonzero balance after sending 1 to itself, so this is an
            // existing holder's cost even for a send of the whole balance. It's only a hint, so
            // a failed read shows none.
            let baseline = reads
                .energy_estimate(sender, contract, transfer(sender, U256::from(1)))
                .await
                .ok();
            (
                estimate,
                false,
                baseline.is_some_and(|baseline| receiver_lacks_token(estimate, baseline)),
            )
        }
    };
    Ok(FeeBasis {
        parameters,
        resources,
        energy_estimate,
        creates_account,
        receiver_lacks_token,
    })
}

/// A receiver's estimate more than 25% above `baseline`, an existing holder's.
const fn receiver_lacks_token(estimate: u64, baseline: u64) -> bool {
    estimate > baseline.saturating_add(baseline / 4)
}

fn build_review(
    draft: &TronSendDraft,
    amount: U256,
    basis: &FeeBasis,
    now_ms: u64,
) -> Result<TronSendReview, TronSendError> {
    if amount.is_zero() {
        return Err(TronSendError::ZeroAmount);
    }
    let (asset, trx_amount) = match draft.asset {
        TronSendAsset::Trx => {
            let amount_sun =
                u64::try_from(amount).map_err(|_| eyre::eyre!("the amount exceeds TRX's range"))?;
            (TronAsset::Trx { amount_sun }, amount_sun)
        }
        TronSendAsset::Trc20(contract) => (
            TronAsset::Trc20 {
                contract,
                amount,
                fee_limit_sun: fee_limit_sun(basis.energy_estimate, basis.parameters.energy_price),
            },
            0,
        ),
    };
    let transfer = TronTransfer {
        sender: draft.sender,
        receiver: draft.receiver,
        asset,
    };
    let (fees, bandwidth_bytes) = transfer_fees(&transfer, basis, now_ms)?;
    Ok(TronSendReview {
        transfer,
        fees,
        parameters: basis.parameters,
        creates_account: basis.creates_account,
        resources: basis.resources,
        bandwidth_bytes,
        receiver_lacks_token: basis.receiver_lacks_token,
        trx_balance: draft.trx_balance,
        missing_trx: missing_trx(draft.trx_balance, trx_amount, fees.expected),
    })
}

/// `transfer`'s fees under `basis`, and the bandwidth it's charged for. Only the transaction's
/// size matters here, and a placeholder block encodes to the same size as the reference block
/// chosen at signing.
fn transfer_fees(
    transfer: &TronTransfer,
    basis: &FeeBasis,
    now_ms: u64,
) -> eyre::Result<(TronFees, u64)> {
    let block = ReferenceBlock {
        number: 0,
        hash: B256::ZERO,
        timestamp_ms: now_ms,
    };
    let transaction = UnsignedTransaction::build(transfer, &block, now_ms)?;
    let bandwidth_bytes = bandwidth_bytes(&transaction);
    let fees = tron_fees(
        &basis.parameters,
        &basis.resources,
        &FeeInputs {
            bandwidth_bytes,
            energy_estimate: basis.energy_estimate,
            creates_account: basis.creates_account,
        },
    );
    Ok((fees, bandwidth_bytes))
}

const fn fee_limit(transfer: &TronTransfer) -> u64 {
    match transfer.asset {
        TronAsset::Trx { .. } => 0,
        TronAsset::Trc20 { fee_limit_sun, .. } => fee_limit_sun,
    }
}

#[cfg(test)]
mod tests;
