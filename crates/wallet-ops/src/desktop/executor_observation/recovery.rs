use super::*;
use crate::desktop::executor_discovery::ExecutorErc721;
use crate::public_wallet::PublicErc20;
use crate::vault::{ExecutorRecoveryStepKind, IssuedExecutorRecoveryTransaction};
use crate::walletconnect::WrappedNative;
use broadcaster_core::contracts::railgun::approveCall;

pub(super) fn matches_transaction(
    issued: &IssuedExecutorRecoveryTransaction,
    transaction: &alloy::rpc::types::Transaction,
) -> bool {
    let expected = issued.transaction();
    transaction.tx_hash() == issued.hash()
        && Some(transaction.from()) == expected.from
        && transaction.to().as_ref() == expected.to.as_ref().and_then(|to| to.to())
        && Some(transaction.nonce()) == expected.nonce
        && transaction.value() == expected.value.unwrap_or_default()
        && Some(transaction.input()) == expected.input.input()
}

pub(super) fn effects(
    railgun: Address,
    issued: &IssuedExecutorRecoveryTransaction,
    receipt: &TransactionReceipt,
) -> Result<ExecutorExecutionResult> {
    if !receipt.status() {
        return Ok(ExecutorExecutionResult::Reverted);
    }
    let transaction = issued.transaction();
    let from = transaction
        .from
        .ok_or_else(|| eyre!("recovery source is missing"))?;
    let to = transaction
        .to
        .as_ref()
        .and_then(|to| to.to())
        .copied()
        .ok_or_else(|| eyre!("recovery destination is missing"))?;
    let data = transaction
        .input
        .input()
        .ok_or_else(|| eyre!("recovery calldata is missing"))?;
    let mut logs = receipt
        .logs()
        .iter()
        .filter(|log| log.address() == to && !log.removed);
    let present = match issued.kind() {
        ExecutorRecoveryStepKind::Shield => {
            let shields = shieldCall::abi_decode(data)?._shieldRequests;
            to == railgun
                && !shields.is_empty()
                && shield_effects_present(railgun, &shields, receipt)
        }
        ExecutorRecoveryStepKind::Wrap => {
            WrappedNative::depositCall::abi_decode(data)?;
            logs.any(|log| {
                log.log_decode::<WrappedNative::Deposit>().is_ok_and(|log| {
                    log.inner.data.dst == from
                        && log.inner.data.wad == transaction.value.unwrap_or_default()
                })
            })
        }
        ExecutorRecoveryStepKind::ApproveErc20 => {
            let approval = approveCall::abi_decode(data)?;
            logs.any(|log| {
                log.log_decode::<PublicErc20::Approval>().is_ok_and(|log| {
                    log.inner.data.owner == from
                        && log.inner.data.spender == approval.spender
                        && log.inner.data.value == approval.amount
                })
            })
        }
        ExecutorRecoveryStepKind::ApproveErc721 => {
            let approval = ExecutorErc721::approveCall::abi_decode(data)?;
            logs.any(|log| {
                log.log_decode::<ExecutorErc721::Approval>()
                    .is_ok_and(|log| {
                        log.inner.data.owner == from
                            && log.inner.data.approved == approval.spender
                            && log.inner.data.tokenId == approval.tokenId
                    })
            })
        }
    };
    Ok(if present {
        ExecutorExecutionResult::Executed
    } else {
        ExecutorExecutionResult::MissingEffects
    })
}
