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
    TronBlockReads, TronReads, TronSendHandoff, TronSendOutcome, confirm_tron_send,
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
use crate::public_wallet::PublicErc20;
use crate::vault::{NonEvmAddressBookEntry, TronAccountMetadata};
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
        self.api.account_resources(account).await
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

/// Signs `review` once `signer` resolves, hands it to an endpoint and follows it to its outcome.
/// `signer` performs authorization and any hardware approval; the transaction is built after
/// it, on the latest block. Progress and session events follow EVM Public Send's, all on the
/// Send step.
pub async fn submit_tron_send<R: TronSendReads + TronBlockReads + ?Sized>(
    reads: &R,
    review: &TronSendReview,
    signer: impl Future<Output = eyre::Result<PrivateKeySigner>> + Send,
    event_tx: Option<&PublicActionSessionEventSender>,
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
    let signed_fees = transfer_fees(&transfer, &basis, now_ms)?;
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
    let outcome = confirm_tron_send(reads, &handoff).await;
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
    let (energy_estimate, creates_account) = match asset {
        TronSendAsset::Trx => (0, !reads.account_exists(receiver).await?),
        TronSendAsset::Trc20(contract) => {
            let data = PublicErc20::transferCall {
                recipient: receiver,
                amount,
            }
            .abi_encode();
            (
                reads.energy_estimate(sender, contract, data.into()).await?,
                false,
            )
        }
    };
    Ok(FeeBasis {
        parameters,
        resources,
        energy_estimate,
        creates_account,
    })
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
    let fees = transfer_fees(&transfer, basis, now_ms)?;
    Ok(TronSendReview {
        transfer,
        fees,
        parameters: basis.parameters,
        creates_account: basis.creates_account,
        trx_balance: draft.trx_balance,
        missing_trx: missing_trx(draft.trx_balance, trx_amount, fees.expected),
    })
}

/// `transfer`'s fees under `basis`. Only the transaction's size matters here, and a placeholder
/// block encodes to the same size as the reference block chosen at signing.
fn transfer_fees(transfer: &TronTransfer, basis: &FeeBasis, now_ms: u64) -> eyre::Result<TronFees> {
    let block = ReferenceBlock {
        number: 0,
        hash: B256::ZERO,
        timestamp_ms: now_ms,
    };
    let transaction = UnsignedTransaction::build(transfer, &block, now_ms)?;
    Ok(tron_fees(
        &basis.parameters,
        &basis.resources,
        &FeeInputs {
            bandwidth_bytes: bandwidth_bytes(&transaction),
            energy_estimate: basis.energy_estimate,
            creates_account: basis.creates_account,
        },
    ))
}

const fn fee_limit(transfer: &TronTransfer) -> u64 {
    match transfer.asset {
        TronAsset::Trx { .. } => 0,
        TronAsset::Trc20 { fee_limit_sun, .. } => fee_limit_sun,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tron::confirmation::TronBlock;
    use crate::tron::http_api::TronTransactionInfo;
    use crate::tron::transaction::EXPIRATION_WINDOW_MS;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    const TRX: u64 = 1_000_000;
    const START_MS: u64 = 1_793_459_319_000;
    const BLOCK_MS: u64 = 3_000;
    const USDT: Address = Address::repeat_byte(0xa6);
    const PARAMETERS: ChainFeeParameters = ChainFeeParameters {
        bandwidth_price: 1_000,
        energy_price: 100,
        create_account_fee: TRX / 10,
        create_account_system_fee: TRX,
        create_account_bandwidth_rate: 1,
    };

    /// A chain whose latest block is at the clock, less `stale_ms`. Every broadcast transaction
    /// is included, successfully and without logs, in the first solidified block after it.
    struct Chain {
        now_ms: AtomicU64,
        stale_ms: u64,
        sender_exists: bool,
        receiver_exists: bool,
        energy_estimate: AtomicU64,
        broadcast_answer: Result<TronBroadcast, RpcBrokerError>,
        broadcasts: Mutex<Vec<SignedTransaction>>,
    }

    impl Chain {
        fn new() -> Self {
            Self {
                now_ms: AtomicU64::new(START_MS),
                stale_ms: 0,
                sender_exists: true,
                receiver_exists: true,
                energy_estimate: AtomicU64::new(65_000),
                broadcast_answer: Ok(TronBroadcast {
                    accepted: true,
                    code: None,
                }),
                broadcasts: Mutex::new(Vec::new()),
            }
        }

        fn latest(&self) -> TronBlock {
            let timestamp_ms = self.now_ms() - self.stale_ms;
            TronBlock {
                number: timestamp_ms / BLOCK_MS,
                hash: B256::repeat_byte(1),
                timestamp_ms,
                transactions: Vec::new(),
            }
        }

        fn broadcast_txids(&self) -> Vec<B256> {
            self.broadcasts
                .lock()
                .unwrap()
                .iter()
                .map(SignedTransaction::txid)
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl TronSendReads for Chain {
        async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError> {
            Ok(if account == signer().address() {
                self.sender_exists
            } else {
                self.receiver_exists
            })
        }

        async fn account_resources(&self, _: Address) -> Result<AccountResources, RpcBrokerError> {
            Ok(AccountResources::default())
        }

        async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError> {
            Ok(PARAMETERS)
        }

        async fn energy_estimate(
            &self,
            _: Address,
            _: Address,
            _: Bytes,
        ) -> Result<u64, RpcBrokerError> {
            Ok(self.energy_estimate.load(Ordering::SeqCst))
        }

        async fn latest_block(&self) -> Result<ReferenceBlock, RpcBrokerError> {
            let block = self.latest();
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
            assert!(
                self.now_ms() < transaction.expiration_ms(),
                "expired broadcast"
            );
            self.broadcasts.lock().unwrap().push(transaction.clone());
            self.broadcast_answer.clone()
        }

        fn now_ms(&self) -> u64 {
            self.now_ms.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl TronBlockReads for Chain {
        async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError> {
            let latest = self.latest();
            Ok(TronBlock {
                number: latest.number + 1,
                transactions: self.broadcast_txids(),
                ..latest
            })
        }

        async fn block(&self, number: u64) -> Result<TronBlock, RpcBrokerError> {
            Ok(TronBlock {
                number,
                ..self.latest()
            })
        }

        async fn transaction_infos(
            &self,
            _: u64,
        ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
            Ok(self
                .broadcast_txids()
                .into_iter()
                .map(|id| TronTransactionInfo {
                    id,
                    succeeded: true,
                    logs: Vec::new(),
                    fee_sun: 0,
                })
                .collect())
        }
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap()
    }

    fn draft(asset: TronSendAsset, amount: TronSendAmount, trx_balance: u64) -> TronSendDraft {
        TronSendDraft {
            sender: signer().address(),
            receiver: Address::repeat_byte(0x22),
            asset,
            amount,
            trx_balance,
        }
    }

    async fn submit(chain: &Chain, review: &TronSendReview) -> TronSendSubmission {
        submit_tron_send(chain, review, async { Ok(signer()) }, None, || true, |_| {})
            .await
            .unwrap()
    }

    #[test]
    fn receivers_other_than_base58_tron_are_refused() {
        assert_eq!(
            parse_tron_receiver(" TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t ").unwrap(),
            alloy::primitives::address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c")
        );
        for text in [
            "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c",
            "41a614f803b6fd780986a42c78ec9c7f77e6ded13c",
            "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy",
        ] {
            assert!(
                matches!(
                    parse_tron_receiver(text),
                    Err(TronSendError::NotATronAddress)
                ),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn a_sender_not_on_chain_cannot_send() {
        let chain = Chain {
            sender_exists: false,
            ..Chain::new()
        };
        let draft = draft(
            TronSendAsset::Trx,
            TronSendAmount::Exact(U256::from(TRX)),
            TRX,
        );
        assert!(matches!(
            prepare_tron_send(&chain, &draft).await,
            Err(TronSendError::SenderMissing)
        ));
    }

    #[tokio::test]
    async fn trx_to_the_sender_is_refused_before_any_read_but_a_token_is_not() {
        // A missing sender would fail the first read, so `SelfTransfer` shows none ran.
        let chain = Chain {
            sender_exists: false,
            ..Chain::new()
        };
        for amount in [TronSendAmount::Exact(U256::from(TRX)), TronSendAmount::Max] {
            let draft = TronSendDraft {
                receiver: signer().address(),
                ..draft(TronSendAsset::Trx, amount, 10 * TRX)
            };
            assert!(matches!(
                prepare_tron_send(&chain, &draft).await,
                Err(TronSendError::SelfTransfer)
            ));
        }

        let token = TronSendDraft {
            receiver: signer().address(),
            ..draft(
                TronSendAsset::Trc20(USDT),
                TronSendAmount::Exact(U256::from(1)),
                100 * TRX,
            )
        };
        let review = prepare_tron_send(&Chain::new(), &token).await.unwrap();
        assert_eq!(review.transfer.receiver, review.transfer.sender);
    }

    #[tokio::test]
    async fn trx_to_a_new_account_states_creation_max_and_missing_trx() {
        let chain = Chain {
            receiver_exists: false,
            ..Chain::new()
        };
        let creation = TRX + TRX / 10;
        let max_draft = draft(TronSendAsset::Trx, TronSendAmount::Max, 5 * TRX);
        let max = prepare_tron_send(&chain, &max_draft).await.unwrap();
        assert!(max.creates_account);
        assert_eq!(max.fees.maximum, creation);
        assert_eq!(
            max.transfer.asset,
            TronAsset::Trx {
                amount_sun: 5 * TRX - creation
            }
        );
        assert_eq!(max.missing_trx, None);

        let exact = draft(
            TronSendAsset::Trx,
            TronSendAmount::Exact(U256::from(5 * TRX)),
            5 * TRX,
        );
        let review = prepare_tron_send(&chain, &exact).await.unwrap();
        assert_eq!(review.missing_trx, Some(creation));
    }

    #[tokio::test(start_paused = true)]
    async fn a_higher_maximum_at_signing_returns_a_new_review_unsigned() {
        let chain = Chain::new();
        let draft = draft(
            TronSendAsset::Trc20(USDT),
            TronSendAmount::Exact(U256::from(1)),
            100 * TRX,
        );
        let review = prepare_tron_send(&chain, &draft).await.unwrap();
        chain.energy_estimate.store(70_000, Ordering::SeqCst);
        let TronSendSubmission::Rereview(rereview) = submit(&chain, &review).await else {
            panic!("signed despite a higher maximum");
        };
        assert!(rereview.fees.maximum > review.fees.maximum);
        assert!(chain.broadcasts.lock().unwrap().is_empty());

        // The new review signs within its own bounds.
        let TronSendSubmission::Decided { handoff, .. } = submit(&chain, &rereview).await else {
            panic!("unchanged fees asked for another review");
        };
        assert_eq!(handoff.transfer, rereview.transfer);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_approval_signs_on_a_fresh_block_and_any_broadcast_answer_waits_for_blocks() {
        let chain = Chain {
            broadcast_answer: Err(RpcBrokerError::Timeout),
            ..Chain::new()
        };
        let draft = draft(
            TronSendAsset::Trx,
            TronSendAmount::Exact(U256::from(TRX)),
            10 * TRX,
        );
        let review = prepare_tron_send(&chain, &draft).await.unwrap();
        let approval = async {
            chain
                .now_ms
                .fetch_add(2 * EXPIRATION_WINDOW_MS, Ordering::SeqCst);
            Ok(signer())
        };
        let mut updates = Vec::new();
        let submission = submit_tron_send(
            &chain,
            &review,
            approval,
            None,
            || true,
            |update| {
                updates.push(update);
            },
        )
        .await
        .unwrap();
        let TronSendSubmission::Decided { handoff, outcome } = submission else {
            panic!("unchanged fees asked for another review");
        };
        assert_eq!(handoff.reference_block, chain.latest().number);
        assert_eq!(chain.broadcast_txids(), [handoff.txid]);
        assert!(matches!(
            outcome,
            TronSendOutcome::Complete { amount, .. } if amount == U256::from(TRX)
        ));
        assert!(updates.iter().any(|update| update.message.is_some()
            && update.status == PublicActionProgressStatus::Pending));
        assert_eq!(
            updates.last().unwrap().status,
            PublicActionProgressStatus::Done
        );
    }

    #[tokio::test]
    async fn an_expired_transaction_is_never_handed_off() {
        // An endpoint whose latest block is older than the expiration window.
        let chain = Chain {
            stale_ms: EXPIRATION_WINDOW_MS + BLOCK_MS,
            ..Chain::new()
        };
        let draft = draft(
            TronSendAsset::Trx,
            TronSendAmount::Exact(U256::from(TRX)),
            10 * TRX,
        );
        let review = prepare_tron_send(&chain, &draft).await.unwrap();
        let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let result = submit_tron_send(
            &chain,
            &review,
            async { Ok(signer()) },
            Some(&event_tx),
            || true,
            |_| {},
        )
        .await;
        assert!(matches!(result, Err(TronSendError::Expired)));
        assert!(chain.broadcasts.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err(), "handoff announced");
    }

    /// A Tron node whose latest block is the clock's, and whose solidified head at the same
    /// height is past any expiration, so a send is dropped without a transaction lookup.
    fn json_rpc_node() -> crate::rpc_broker::tests::RpcResponder {
        // A recorded mainnet block, so the broker's block validation accepts it.
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/confirmation.json")).unwrap();
        let recorded = fixture["blocks"]["87595297"].clone();
        Arc::new(move |request| {
            let result = if request["method"] == "eth_chainId" {
                json!("0x2b6653dc")
            } else {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let timestamp = if request["params"][0] == "finalized" {
                    now + 86_400
                } else {
                    now
                };
                let mut block = recorded.clone();
                block["number"] = json!(U64::from(100));
                block["hash"] = json!(B256::repeat_byte(1));
                block["timestamp"] = json!(U64::from(timestamp));
                block["transactions"] = json!([]);
                block
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        })
    }

    /// An HTTP API on which every account exists without resources and every broadcast is
    /// accepted.
    fn http_api_node() -> crate::rpc_broker::tests::RpcResponder {
        let fixtures: Value = serde_json::from_str(include_str!("fixtures/http_api.json")).unwrap();
        Arc::new(move |request| {
            if request.get("num").is_some() {
                fixtures["getblockbynum_0"].clone()
            } else if request.get("address").is_some() {
                json!({ "address": request["address"] })
            } else if request.get("transaction").is_some() {
                json!({ "result": true })
            } else {
                fixtures["getchainparameters"].clone()
            }
        })
    }

    #[tokio::test]
    async fn each_send_reads_and_broadcasts_only_over_its_own_operation_client() {
        use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
        use crate::settings::{TronEndpointSetting, TronSettings};
        use crate::tron::address::encode_tron_address;
        use crate::{HttpContext, OperationHttpClient, OperationNetworkIsolation};

        let (rpc, rpc_log, _rpc) = spawn_recording_rpc_mock(json_rpc_node()).await;
        let (api, api_log, _api) = spawn_recording_rpc_mock(http_api_node()).await;
        let route = crate::tron::tron_rpc_route(&TronSettings {
            endpoints: Some(vec![TronEndpointSetting {
                json_rpc_url: rpc.to_string(),
                http_api_url: Some(api.to_string()),
                ..TronEndpointSetting::default()
            }]),
            ..TronSettings::default()
        })
        .unwrap();
        let http = HttpContext::direct_for_tests();
        // Each send has its own receiver, so its receiver reads can be told apart.
        let receiver = |tag: &str| Address::repeat_byte(if tag == "a" { 0x22 } else { 0x33 });
        let send = |tag: &'static str| {
            let reads = TronReads::new(
                http.clone(),
                &route,
                OperationHttpClient::for_tests(
                    tagged_reqwest_client(tag),
                    OperationNetworkIsolation::Dedicated,
                ),
            );
            let draft = TronSendDraft {
                receiver: receiver(tag),
                ..draft(
                    TronSendAsset::Trx,
                    TronSendAmount::Exact(U256::from(TRX)),
                    100 * TRX,
                )
            };
            async move {
                let review = prepare_tron_send(&reads, &draft).await.unwrap();
                let submission = submit_tron_send(
                    &reads,
                    &review,
                    async { Ok(signer()) },
                    None,
                    || true,
                    |_| {},
                )
                .await
                .unwrap();
                let TronSendSubmission::Decided { outcome, .. } = submission else {
                    panic!("unchanged fees asked for another review");
                };
                assert_eq!(outcome, TronSendOutcome::Dropped);
            }
        };
        tokio::join!(send("a"), send("b"));

        // Identity checks go over the shared clients and name no account.
        for (api_name, log) in [("JSON-RPC", &rpc_log), ("HTTP API", &api_log)] {
            let requests: Vec<_> = log
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, body)| body["method"] != "eth_chainId" && body["num"] != 0)
                .cloned()
                .collect();
            for tag in ["a", "b"] {
                assert!(
                    requests
                        .iter()
                        .any(|(sent, _)| sent.as_deref() == Some(tag)),
                    "{api_name}: no request of {tag}"
                );
            }
            for (sent, body) in &requests {
                assert!(
                    matches!(sent.as_deref(), Some("a" | "b")),
                    "{api_name}: {body} over {sent:?}"
                );
            }
        }
        for tag in ["a", "b"] {
            let address = encode_tron_address(receiver(tag));
            let reads: Vec<_> = api_log
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, body)| body["address"] == address.as_str())
                .map(|(sent, _)| sent.clone())
                .collect();
            assert!(!reads.is_empty(), "{tag}'s receiver unread");
            assert!(
                reads.iter().all(|sent| sent.as_deref() == Some(tag)),
                "{tag}'s receiver read over {reads:?}"
            );
        }
    }
}
