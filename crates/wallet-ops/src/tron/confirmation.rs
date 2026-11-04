//! Tron Send confirmation from solidified blocks.
//!
//! Solidified blocks can't be replaced, so each is read once, in order from the reference block,
//! and no block above the solidified head is read. No request names the transaction id: blocks
//! are read by number and their outcomes by block number.

use super::http_api::{TronHttpApi, TronTransactionInfo};
use super::transaction::{TronAsset, TronTransfer};
use crate::public_wallet::PublicErc20;
use crate::{
    HttpContext, OperationHttpClient, RpcBrokerError, RpcChainRoute, RpcRead, RpcResult, RpcRoute,
    RpcSubmission, RpcTrafficClass, WalletRpcOrigin,
};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256, U64, U256};
use alloy::sol_types::SolEvent as _;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// Tron produces a block every 3 seconds.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A block's number, hash, time and transaction ids, from `eth_getBlockByNumber(n, false)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TronBlock {
    pub number: u64,
    pub hash: B256,
    pub timestamp_ms: u64,
    /// Tron transaction ids.
    pub transactions: Vec<B256>,
}

/// Alloy's RPC block can't decode Tron's, whose `stateRoot` is `0x`, so only these fields are
/// read.
#[derive(Deserialize)]
struct BlockWire {
    number: U64,
    hash: B256,
    /// Seconds, unlike the HTTP API's milliseconds.
    timestamp: U64,
    transactions: Vec<B256>,
}

fn decode_block(value: Value) -> Result<TronBlock, RpcBrokerError> {
    let wire: BlockWire =
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    Ok(TronBlock {
        number: wire.number.to(),
        hash: wire.hash,
        timestamp_ms: wire
            .timestamp
            .to::<u64>()
            .checked_mul(1000)
            .ok_or(RpcBrokerError::InvalidResponse)?,
        transactions: wire.transactions,
    })
}

/// The reads confirmation makes.
#[async_trait::async_trait]
pub trait TronBlockReads: Send + Sync {
    /// The solidified head: `eth_getBlockByNumber("finalized", false)`.
    async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError>;
    /// `eth_getBlockByNumber(number, false)`. Only called at or below the solidified head.
    async fn block(&self, number: u64) -> Result<TronBlock, RpcBrokerError>;
    /// `gettransactioninfobyblocknum`.
    async fn transaction_infos(
        &self,
        number: u64,
    ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError>;
}

/// A Send's reads over Tron's route: JSON-RPC through the broker and the HTTP API. Everything
/// belongs to a Send, so reads are Sensitive and broadcast is Submission.
#[derive(Clone)]
pub struct TronReads {
    http: HttpContext,
    route: RpcChainRoute,
    pub(super) api: TronHttpApi,
}

impl TronReads {
    /// Every read and the broadcast go over `client`, the Send's own operation route.
    #[must_use]
    pub fn new(http: HttpContext, route: &RpcChainRoute, client: OperationHttpClient) -> Self {
        let route = route.isolated(client);
        Self {
            api: TronHttpApi::new(http.clone(), route.clone()),
            http,
            route,
        }
    }

    pub(super) async fn json_rpc(&self, read: RpcRead) -> Result<Value, RpcBrokerError> {
        let route = self.route.for_class(RpcTrafficClass::Sensitive)?;
        self.http
            .rpc_broker()
            .submit(RpcSubmission::new(
                RpcRoute::from(route),
                vec![read],
                WalletRpcOrigin::PublicWallet.into(),
            ))
            .await?
            .into_iter()
            .next()
            .ok_or(RpcBrokerError::InvalidResponse)?
            .map(RpcResult::into_value)
    }

    pub(super) async fn block_at(
        &self,
        block: BlockNumberOrTag,
    ) -> Result<TronBlock, RpcBrokerError> {
        decode_block(
            self.json_rpc(RpcRead::get_block_by_number(block, false))
                .await?,
        )
    }
}

#[async_trait::async_trait]
impl TronBlockReads for TronReads {
    async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError> {
        self.block_at(BlockNumberOrTag::Finalized).await
    }

    async fn block(&self, number: u64) -> Result<TronBlock, RpcBrokerError> {
        self.block_at(BlockNumberOrTag::Number(number)).await
    }

    async fn transaction_infos(
        &self,
        number: u64,
    ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
        self.api.transaction_infos(number).await
    }
}

/// A signed transaction handed to an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TronSendHandoff {
    pub transfer: TronTransfer,
    pub txid: B256,
    pub reference_block: u64,
    pub expiration_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TronSendOutcome {
    /// Included and the asset moved: the TRX sent in sun, or the amount the token's `Transfer`
    /// log reports. `block` is the including block; a balance read below it predates the send.
    Complete { amount: U256, block: u64 },
    /// Included without moving the asset, burning the transaction's fee.
    Failed {
        reason: TronSendFailure,
        trx_burned_sun: u64,
        block: u64,
    },
    /// A solidified block later than the expiration was read without the transaction, so no
    /// TRX was spent.
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TronSendFailure {
    #[error("the transaction failed")]
    Failed,
    /// Success without a `Transfer` log of the token from the sender to the receiver. A token's
    /// return value can't decide this: Tron's USDT returns `false` while moving tokens.
    #[error("the token didn't move")]
    TokenDidNotMove,
}

/// Follows the solidified head until a block includes the handoff or passes its expiration.
/// Every handoff waits for this, whatever the endpoint answered. Failed reads are retried at the
/// next poll.
pub async fn confirm_tron_send<R: TronBlockReads + ?Sized>(
    reads: &R,
    handoff: &TronSendHandoff,
) -> TronSendOutcome {
    confirm_tron_send_at(reads, handoff).await.0
}

/// [`confirm_tron_send`] with the timestamp in milliseconds of the block that decided it.
pub(crate) async fn confirm_tron_send_at<R: TronBlockReads + ?Sized>(
    reads: &R,
    handoff: &TronSendHandoff,
) -> (TronSendOutcome, u64) {
    let mut next = handoff.reference_block;
    loop {
        if let Ok(Some(decided)) = scan_solidified(reads, handoff, &mut next).await {
            return decided;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Reads blocks from `next` up to the solidified head, advancing `next` past each one read.
/// A decided outcome comes with its block's timestamp in milliseconds.
async fn scan_solidified<R: TronBlockReads + ?Sized>(
    reads: &R,
    handoff: &TronSendHandoff,
    next: &mut u64,
) -> Result<Option<(TronSendOutcome, u64)>, RpcBrokerError> {
    let head = reads.solidified_head().await?;
    while *next <= head.number {
        let block = if *next == head.number {
            head.clone()
        } else {
            reads.block(*next).await?
        };
        if block.number != *next {
            return Err(RpcBrokerError::InvalidResponse);
        }
        if block.transactions.contains(&handoff.txid) {
            let info = reads
                .transaction_infos(block.number)
                .await?
                .into_iter()
                .find(|info| info.id == handoff.txid)
                .ok_or(RpcBrokerError::InvalidResponse)?;
            let decided = outcome(&handoff.transfer, &info, block.number);
            return Ok(Some((decided, block.timestamp_ms)));
        }
        if block.timestamp_ms > handoff.expiration_ms {
            return Ok(Some((TronSendOutcome::Dropped, block.timestamp_ms)));
        }
        *next += 1;
    }
    Ok(None)
}

fn outcome(transfer: &TronTransfer, info: &TronTransactionInfo, block: u64) -> TronSendOutcome {
    let failed = |reason| TronSendOutcome::Failed {
        reason,
        trx_burned_sun: info.fee_sun,
        block,
    };
    if !info.succeeded {
        return failed(TronSendFailure::Failed);
    }
    match transfer.asset {
        TronAsset::Trx { amount_sun } => TronSendOutcome::Complete {
            amount: U256::from(amount_sun),
            block,
        },
        TronAsset::Trc20 { contract, .. } => info
            .logs
            .iter()
            .filter(|log| log.address == contract)
            .filter_map(|log| PublicErc20::Transfer::decode_log_data(&log.data).ok())
            .find(|event| event.from == transfer.sender && event.to == transfer.receiver)
            .map_or_else(
                || failed(TronSendFailure::TokenDidNotMove),
                |event| TronSendOutcome::Complete {
                    amount: event.value,
                    block,
                },
            ),
    }
}

/// The accounts to refresh after any outcome: the sender, and the receiver if it is one of the
/// `visible` Tron accounts.
#[must_use]
pub fn tron_send_refresh_accounts(transfer: &TronTransfer, visible: &[Address]) -> Vec<Address> {
    let mut accounts = vec![transfer.sender];
    if transfer.receiver != transfer.sender && visible.contains(&transfer.receiver) {
        accounts.push(transfer.receiver);
    }
    accounts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tron::http_api::decode_transaction_infos;
    use alloy::primitives::{address, b256};
    use std::sync::Mutex;

    const USDT: Address = address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
    /// The recorded blocks are 87595297 to 87595299, three seconds apart.
    const INCLUDING_BLOCK: u64 = 87_595_298;
    const INCLUDING_BLOCK_MS: u64 = 1_793_459_319_000;

    /// Serves the recorded blocks and records each request. A block read above the last
    /// solidified head served fails the test.
    struct Recorded {
        fixture: Value,
        heads: Mutex<Vec<u64>>,
        served_head: Mutex<u64>,
        requests: Mutex<Vec<String>>,
    }

    impl Recorded {
        fn new(heads: &[u64]) -> Self {
            Self {
                fixture: serde_json::from_str(include_str!("fixtures/confirmation.json")).unwrap(),
                heads: Mutex::new(heads.iter().rev().copied().collect()),
                served_head: Mutex::new(0),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn recorded_block(&self, number: u64) -> TronBlock {
            decode_block(self.fixture["blocks"][number.to_string()].clone()).unwrap()
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl TronBlockReads for Recorded {
        async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError> {
            let number = {
                let mut heads = self.heads.lock().unwrap();
                if heads.len() > 1 {
                    heads.pop().unwrap()
                } else {
                    heads[0]
                }
            };
            *self.served_head.lock().unwrap() = number;
            self.requests.lock().unwrap().push("finalized".into());
            Ok(self.recorded_block(number))
        }

        async fn block(&self, number: u64) -> Result<TronBlock, RpcBrokerError> {
            assert!(
                number <= *self.served_head.lock().unwrap(),
                "unsolidified read"
            );
            self.requests
                .lock()
                .unwrap()
                .push(format!("block {number}"));
            Ok(self.recorded_block(number))
        }

        async fn transaction_infos(
            &self,
            number: u64,
        ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
            assert!(
                number <= *self.served_head.lock().unwrap(),
                "unsolidified read"
            );
            self.requests
                .lock()
                .unwrap()
                .push(format!("infos {number}"));
            decode_transaction_infos(self.fixture[format!("transaction_infos_{number}")].clone())
        }
    }

    fn handoff(
        txid: B256,
        asset: TronAsset,
        sender: Address,
        receiver: Address,
    ) -> TronSendHandoff {
        TronSendHandoff {
            transfer: TronTransfer {
                sender,
                receiver,
                asset,
            },
            txid,
            reference_block: INCLUDING_BLOCK - 1,
            expiration_ms: INCLUDING_BLOCK_MS + 60_000,
        }
    }

    fn usdt(amount: u64) -> TronAsset {
        TronAsset::Trc20 {
            contract: USDT,
            amount: U256::from(amount),
            fee_limit_sun: 100_000_000,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn outcomes_come_from_the_including_solidified_block() {
        // A USDT transfer whose `transfer` returned false (`contractResult` zero) but which
        // logged the Transfer.
        let usdt_txid = b256!("02157d962b220e558bde03fb2a2efec13bf2078936b1f610884d9091086d0e2d");
        let usdt_sender = address!("0xe85a3bb3997b963ebc6d227ae4998272ba25e843");
        let usdt_receiver = address!("0x869f159b652aeb389f00d92734a7381ca143ba6b");
        let cases = [
            (
                "USDT moved",
                handoff(usdt_txid, usdt(1), usdt_sender, usdt_receiver),
                TronSendOutcome::Complete {
                    amount: U256::from(0x0253_17c0_u64),
                    block: INCLUDING_BLOCK,
                },
            ),
            (
                "success without a matching Transfer log",
                handoff(usdt_txid, usdt(1), usdt_sender, Address::repeat_byte(9)),
                TronSendOutcome::Failed {
                    reason: TronSendFailure::TokenDidNotMove,
                    trx_burned_sun: 345_000,
                    block: INCLUDING_BLOCK,
                },
            ),
            (
                "out of energy",
                handoff(
                    b256!("8442ac1bc6f82ca5d7cf0cefb8e951180fbcaa08dfdce5f2b81f9543ae5eef11"),
                    usdt(1),
                    usdt_sender,
                    usdt_receiver,
                ),
                TronSendOutcome::Failed {
                    reason: TronSendFailure::Failed,
                    trx_burned_sun: 815_400,
                    block: INCLUDING_BLOCK,
                },
            ),
            (
                "TRX",
                handoff(
                    b256!("000c81fa105ff13b9b36cbcfd47319a60fe38175fc575298eeab1d1d2210ab19"),
                    TronAsset::Trx { amount_sun: 5 },
                    usdt_sender,
                    usdt_receiver,
                ),
                TronSendOutcome::Complete {
                    amount: U256::from(5),
                    block: INCLUDING_BLOCK,
                },
            ),
        ];
        for (name, handoff, expected) in cases {
            // The head first sits on the reference block, so the next poll reads the rest.
            let reads = Recorded::new(&[INCLUDING_BLOCK - 1, INCLUDING_BLOCK + 1]);
            assert_eq!(
                confirm_tron_send(&reads, &handoff).await,
                expected,
                "{name}"
            );
            let requests = reads.requests();
            assert_eq!(
                requests,
                ["finalized", "finalized", "block 87595298", "infos 87595298"],
                "{name}"
            );
            let txid = alloy::hex::encode(handoff.txid);
            assert!(
                requests.iter().all(|request| !request.contains(&txid)),
                "{name}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_once_a_solidified_block_passes_the_expiration() {
        let mut handoff = handoff(
            B256::repeat_byte(7),
            TronAsset::Trx { amount_sun: 5 },
            Address::repeat_byte(1),
            Address::repeat_byte(2),
        );
        handoff.expiration_ms = INCLUDING_BLOCK_MS - 1;
        let reads = Recorded::new(&[INCLUDING_BLOCK + 1]);
        assert_eq!(
            confirm_tron_send(&reads, &handoff).await,
            TronSendOutcome::Dropped
        );
        // 87595297 is before the expiration and doesn't decide; 87595298 is after it.
        assert_eq!(
            reads.requests(),
            ["finalized", "block 87595297", "block 87595298"]
        );

        let visible = [Address::repeat_byte(2), Address::repeat_byte(3)];
        assert_eq!(
            tron_send_refresh_accounts(&handoff.transfer, &visible),
            [Address::repeat_byte(1), Address::repeat_byte(2)]
        );
        handoff.transfer.receiver = Address::repeat_byte(4);
        assert_eq!(
            tron_send_refresh_accounts(&handoff.transfer, &visible),
            [Address::repeat_byte(1)]
        );
    }
}
