//! Tron transfers built and signed locally.
//!
//! Only the protobuf fields this wallet sends are declared, with java-tron's field numbers from
//! `core/Tron.proto`, `core/contract/balance_contract.proto` and `smart_contract.proto`. Every
//! address inside a transaction is Tron's 21-byte form, `0x41` followed by the 20-byte address.

use alloy::primitives::{Address, B256, U256};
use alloy::signers::SignerSync as _;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall as _;
use eyre::{Result, WrapErr as _, ensure};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

use super::address::tron_address_bytes;
use crate::public_wallet::PublicErc20;

/// How long after its reference block's time a transaction stays valid.
pub const EXPIRATION_WINDOW_MS: u64 = 5 * 60 * 1000;

const TRANSFER_CONTRACT: i32 = 1;
const TRIGGER_SMART_CONTRACT: i32 = 31;
const TRANSFER_CONTRACT_URL: &str = "type.googleapis.com/protocol.TransferContract";
const TRIGGER_SMART_CONTRACT_URL: &str = "type.googleapis.com/protocol.TriggerSmartContract";

/// `Transaction`. `ret` (5) is never sent.
#[derive(Clone, PartialEq, Eq, prost::Message)]
struct Transaction {
    #[prost(message, optional, tag = "1")]
    raw_data: Option<TransactionRaw>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    signature: Vec<Vec<u8>>,
}

/// `Transaction.raw`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
struct TransactionRaw {
    #[prost(bytes = "vec", tag = "1")]
    ref_block_bytes: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    ref_block_hash: Vec<u8>,
    #[prost(int64, tag = "8")]
    expiration: i64,
    #[prost(message, repeated, tag = "11")]
    contract: Vec<Contract>,
    #[prost(int64, tag = "14")]
    timestamp: i64,
    #[prost(int64, tag = "18")]
    fee_limit: i64,
}

/// `Transaction.Contract`; `type` is the `ContractType` enum's number.
#[derive(Clone, PartialEq, Eq, prost::Message)]
struct Contract {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(message, optional, tag = "2")]
    parameter: Option<Any>,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
struct TransferContract {
    #[prost(bytes = "vec", tag = "1")]
    owner_address: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    to_address: Vec<u8>,
    #[prost(int64, tag = "3")]
    amount: i64,
}

/// `TriggerSmartContract`. `call_value` (3) is always zero, so it is never encoded.
#[derive(Clone, PartialEq, Eq, prost::Message)]
struct TriggerSmartContract {
    #[prost(bytes = "vec", tag = "1")]
    owner_address: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    contract_address: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    data: Vec<u8>,
}

/// The block a transaction references, read right before signing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceBlock {
    pub number: u64,
    pub hash: B256,
    pub timestamp_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TronAsset {
    Trx {
        amount_sun: u64,
    },
    /// A TRC-20 `transfer(receiver, amount)`, with `fee_limit` capping the energy it can burn.
    Trc20 {
        contract: Address,
        amount: U256,
        fee_limit_sun: u64,
    },
}

/// Addresses are the 20 bytes after Tron's `0x41` prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TronTransfer {
    pub sender: Address,
    pub receiver: Address,
    pub asset: TronAsset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    sender: Address,
    raw: TransactionRaw,
}

impl UnsignedTransaction {
    /// Builds `transfer` against `block`, expiring [`EXPIRATION_WINDOW_MS`] after the block.
    pub fn build(
        transfer: &TronTransfer,
        block: &ReferenceBlock,
        timestamp_ms: u64,
    ) -> Result<Self> {
        let owner_address = tron_address_bytes(transfer.sender).to_vec();
        let (contract_type, type_url, value, fee_limit) = match transfer.asset {
            TronAsset::Trx { amount_sun } => (
                TRANSFER_CONTRACT,
                TRANSFER_CONTRACT_URL,
                TransferContract {
                    owner_address,
                    to_address: tron_address_bytes(transfer.receiver).to_vec(),
                    amount: int64(amount_sun, "TRX amount")?,
                }
                .encode_to_vec(),
                0,
            ),
            TronAsset::Trc20 {
                contract,
                amount,
                fee_limit_sun,
            } => (
                TRIGGER_SMART_CONTRACT,
                TRIGGER_SMART_CONTRACT_URL,
                TriggerSmartContract {
                    owner_address,
                    contract_address: tron_address_bytes(contract).to_vec(),
                    data: PublicErc20::transferCall {
                        recipient: transfer.receiver,
                        amount,
                    }
                    .abi_encode(),
                }
                .encode_to_vec(),
                int64(fee_limit_sun, "fee limit")?,
            ),
        };
        let expiration = block
            .timestamp_ms
            .checked_add(EXPIRATION_WINDOW_MS)
            .ok_or_else(|| eyre::eyre!("reference block time out of range"))?;
        Ok(Self {
            sender: transfer.sender,
            raw: TransactionRaw {
                ref_block_bytes: block.number.to_be_bytes()[6..8].to_vec(),
                ref_block_hash: block.hash[8..16].to_vec(),
                expiration: int64(expiration, "expiration")?,
                contract: vec![Contract {
                    r#type: contract_type,
                    parameter: Some(Any {
                        type_url: type_url.to_owned(),
                        value,
                    }),
                }],
                timestamp: int64(timestamp_ms, "timestamp")?,
                fee_limit,
            },
        })
    }

    /// The protobuf `Transaction.raw` bytes the transaction id and signature cover.
    #[must_use]
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw.encode_to_vec()
    }

    /// sha256 of the raw bytes.
    #[must_use]
    pub fn txid(&self) -> B256 {
        B256::from_slice(&Sha256::digest(self.raw_bytes()))
    }

    #[must_use]
    pub const fn expiration_ms(&self) -> u64 {
        self.raw.expiration.unsigned_abs()
    }

    /// Size of the signed transaction with `ret` cleared, using a 65-byte placeholder signature.
    pub(super) fn signed_len(&self) -> usize {
        Transaction {
            raw_data: Some(self.raw.clone()),
            signature: vec![vec![0; 65]],
        }
        .encoded_len()
    }

    /// Signs the transaction id with the sender's key: `r ‖ s ‖ v`, `v` being 27 plus the
    /// recovery id, as `TronWeb` produces.
    pub fn sign(&self, signer: &PrivateKeySigner) -> Result<SignedTransaction> {
        ensure!(
            signer.address() == self.sender,
            "signing key is not the transaction's sender"
        );
        let txid = self.txid();
        let signature = signer
            .sign_hash_sync(&txid)
            .wrap_err("sign Tron transaction")?;
        Ok(SignedTransaction {
            txid,
            expiration_ms: self.expiration_ms(),
            transaction: Transaction {
                raw_data: Some(self.raw.clone()),
                signature: vec![signature.as_bytes().to_vec()],
            },
        })
    }
}

/// A signed transaction. It has no `Debug`, so its signature can't reach a log.
#[derive(Clone)]
pub struct SignedTransaction {
    txid: B256,
    expiration_ms: u64,
    transaction: Transaction,
}

impl SignedTransaction {
    #[must_use]
    pub const fn txid(&self) -> B256 {
        self.txid
    }

    #[must_use]
    pub const fn expiration_ms(&self) -> u64 {
        self.expiration_ms
    }

    /// The protobuf `Transaction` as hex, for `/wallet/broadcasthex`.
    #[must_use]
    pub fn to_hex(&self) -> String {
        alloy::hex::encode(self.transaction.encode_to_vec())
    }
}

fn int64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value).wrap_err_with(|| format!("{what} exceeds int64"))
}

#[cfg(test)]
mod tests {
    use alloy::hex;
    use alloy::primitives::Signature;
    use serde_json::Value;

    use super::*;
    use crate::tron::address::parse_tron_address;

    const NODE_BUILT: &str = include_str!("fixtures/node_built_transactions.json");

    fn base58(value: &Value) -> Address {
        parse_tron_address(value.as_str().unwrap()).unwrap()
    }

    /// A block giving the node's reference fields. The node expires transactions 60 seconds
    /// after its latest block, so the block time is set to land on the node's expiration.
    fn reference_block(raw: &Value) -> ReferenceBlock {
        let ref_bytes = hex::decode(raw["ref_block_bytes"].as_str().unwrap()).unwrap();
        let mut hash = B256::ZERO;
        hash[8..16].copy_from_slice(&hex::decode(raw["ref_block_hash"].as_str().unwrap()).unwrap());
        ReferenceBlock {
            number: u64::from(u16::from_be_bytes([ref_bytes[0], ref_bytes[1]])),
            hash,
            timestamp_ms: raw["expiration"].as_u64().unwrap() - EXPIRATION_WINDOW_MS,
        }
    }

    /// `bandwidth` is what Tron charges such a transfer: 267 bytes for TRX, 345 for USDT.
    fn assert_matches_node(transfer: &TronTransfer, node: &Value, bandwidth: u64) {
        let raw = &node["raw_data"];
        let built = UnsignedTransaction::build(
            transfer,
            &reference_block(raw),
            raw["timestamp"].as_u64().unwrap(),
        )
        .unwrap();
        assert_eq!(
            hex::encode(built.raw_bytes()),
            node["raw_data_hex"].as_str().unwrap()
        );
        assert_eq!(hex::encode(built.txid()), node["txID"].as_str().unwrap());
        assert_eq!(crate::tron::fees::bandwidth_bytes(&built), bandwidth);
    }

    #[test]
    fn local_encoding_matches_node_built_transactions() {
        let fixture: Value = serde_json::from_str(NODE_BUILT).unwrap();

        let trx = &fixture["createtransaction"];
        assert_matches_node(
            &TronTransfer {
                sender: base58(&trx["request"]["owner_address"]),
                receiver: base58(&trx["request"]["to_address"]),
                asset: TronAsset::Trx {
                    amount_sun: trx["request"]["amount"].as_u64().unwrap(),
                },
            },
            &trx["response"],
            267,
        );

        let token = &fixture["triggersmartcontract"];
        let request = &token["request"];
        // The node's call data is the selector followed by this ABI parameter: receiver, amount.
        let parameter = hex::decode(request["parameter"].as_str().unwrap()).unwrap();
        assert_matches_node(
            &TronTransfer {
                sender: base58(&request["owner_address"]),
                receiver: Address::from_slice(&parameter[12..32]),
                asset: TronAsset::Trc20 {
                    contract: base58(&request["contract_address"]),
                    amount: U256::from_be_slice(&parameter[32..64]),
                    fee_limit_sun: request["fee_limit"].as_u64().unwrap(),
                },
            },
            &token["response"]["transaction"],
            345,
        );
    }

    #[test]
    fn signature_recovers_to_the_sender() {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
        let transfer = TronTransfer {
            sender: signer.address(),
            receiver: Address::repeat_byte(0x22),
            asset: TronAsset::Trx { amount_sun: 1 },
        };
        let block = ReferenceBlock {
            number: 1,
            hash: B256::ZERO,
            timestamp_ms: 0,
        };
        let unsigned = UnsignedTransaction::build(&transfer, &block, 0).unwrap();
        let signed = unsigned.sign(&signer).unwrap();
        assert_eq!(signed.txid(), unsigned.txid());

        let broadcast =
            Transaction::decode(hex::decode(signed.to_hex()).unwrap().as_slice()).unwrap();
        assert_eq!(
            broadcast.raw_data.unwrap().encode_to_vec(),
            unsigned.raw_bytes()
        );
        let [signature] = broadcast.signature.as_slice() else {
            panic!("one signature");
        };
        assert!(matches!(signature[64], 27 | 28));
        let signature = Signature::from_raw(signature).unwrap();
        assert_eq!(
            signature
                .recover_address_from_prehash(&signed.txid())
                .unwrap(),
            signer.address()
        );

        let other = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x33)).unwrap();
        assert!(unsigned.sign(&other).is_err());
    }
}
