//! Mimic Railway native shields from a stealth account. The payer's type-4 transaction sends
//! the value to the account with its delegation, and the account's signed `execute`, with no
//! private transactions, wraps that value and shields it.

use alloy::primitives::{Address, B256, Bytes, Signature, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::executor::{execute_signing_hash, native_shield_action_data};
use broadcaster_core::contracts::railgun::{
    RelayAdapt7702, RelayAdapt7702ActionData, ShieldRequest,
};
use eyre::{Result, WrapErr, eyre};
use railgun_wallet::TransactionCall;

use super::execution::PayloadToSign;
use super::{ExecutorDelivery, ExecutorOwner, IssuedExecutorTransaction};
use crate::vault::{DesktopViewSession, ExecutorOperationId};
use crate::{DesktopPrivateSpendAuthorization, ExecutorAsset};

/// The signed `execute` of a native shield's `action`, with no private transactions.
pub(crate) fn native_shield_execute_calldata(
    action: RelayAdapt7702ActionData,
    nonce: U256,
    signature: Signature,
) -> Bytes {
    RelayAdapt7702::executeCall {
        _transactions: vec![],
        _actionData: action,
        _nonce: nonce,
        _signature: Bytes::copy_from_slice(&signature.as_bytes()),
    }
    .abi_encode()
    .into()
}

impl ExecutorOwner {
    /// Reserves `operation`'s stealth account and signs its shield, with `request`, of the
    /// `amount` that `payer` sends it. The returned transaction is `payer`'s to send.
    pub(crate) async fn issue_native_shield(
        &self,
        operation: ExecutorOperationId,
        view: &DesktopViewSession,
        payer: Address,
        authorization: &DesktopPrivateSpendAuthorization,
        wrapped_native: Address,
        amount: U256,
        request: ShieldRequest,
        purpose_summary: &str,
    ) -> Result<IssuedExecutorTransaction> {
        // The shield's recipient and its stealth account belong to one wallet.
        if !view.is_same_wallet_session(&self.view) {
            return Err(eyre!("the private wallet changed; review the shield again"));
        }
        let prepared = self
            .prepare_authorized_operation(
                operation,
                ExecutorDelivery::SelfBroadcast {
                    sender: payer,
                    sponsored: false,
                },
                authorization,
                &[ExecutorAsset::Native, ExecutorAsset::Erc20(wrapped_native)],
                Some(purpose_summary),
            )
            .await?;
        let context = prepared.context();
        let action = native_shield_action_data(context.executor, request)
            .wrap_err("build the native shield execution")?;
        let hash = execute_signing_hash(
            &[],
            &action,
            context.execution_nonce,
            context.chain_id,
            context.executor,
        );
        self.issue_payload(&prepared, authorization, None, |_| {
            Ok(PayloadToSign {
                hash,
                value: amount,
                inputs: Vec::new(),
                // Railway's native shield is always type 4 with the account's
                // authorization, retries included.
                authorize_delegation: true,
                authorize: Box::new(move |signature| {
                    Ok(TransactionCall {
                        to: context.executor,
                        data: native_shield_execute_calldata(
                            action,
                            context.execution_nonce,
                            signature,
                        ),
                    })
                }),
            })
        })
        .await
    }

    /// The Public send session's hand-off of `issued`: records each signed attempt before
    /// its broadcast.
    pub(crate) fn native_shield_handoff(
        &self,
        issued: &IssuedExecutorTransaction,
    ) -> Result<impl FnMut(B256, &TransactionRequest, &[u8]) -> Result<()> + Send + '_> {
        let identity = self
            .transaction_identity(issued.transaction())?
            .ok_or_else(|| eyre!("the native shield's signed payload is not recorded"))?;
        Ok(move |transaction: B256, _: &TransactionRequest, _: &[u8]| {
            self.record_submission(identity, transaction)
        })
    }
}
