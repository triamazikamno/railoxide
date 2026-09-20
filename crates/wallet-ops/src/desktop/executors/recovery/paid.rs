use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use eyre::{Result, eyre};
use railgun_wallet::tx::{
    BuildError, MixedPrivateActionRequest, MixedPrivateOutputRole, MixedPrivateSend,
    MixedPrivateSendRole,
};
use railgun_wallet::{ProverService, TransactionBuilder, Utxo};

use super::{
    ExecutorOwner, ExecutorRecoveryFunding, PreparedExecutorRecovery, maximum_recovery_gas_limit,
};
use crate::desktop::{
    ApproximateTransactionShape, PUBLIC_BROADCASTER_FEE_ATTEMPTS,
    approximate_public_broadcaster_gas, artifact_source, buffered_gas_price_from_rpc_pool,
    effective_desktop_chain_config, estimate_public_broadcaster_fee_from_rpc_pool,
    query_rpc_pool_with_http_client, submit_public_broadcaster_transaction,
    update_transaction_generation_stage,
};
use crate::poi_contexts::{
    persist_pending_mixed_output_poi_contexts, public_broadcaster_pre_transaction_pois,
};
use crate::vault::ExecutorOperationId;
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorDelivery, PreparedExecutorOperation,
    PublicBroadcasterCandidate, PublicBroadcasterResultKind, TransactionGenerationProgressSender,
    TransactionGenerationStage, WakuClient, WalletSession, broadcaster_fee_amount,
    buffered_public_broadcaster_fee, public_broadcaster_bound_min_gas_price,
    public_broadcaster_service_gas_price,
};

#[derive(Clone)]
pub struct ExecutorRecoveryFeeEstimate {
    broadcaster: PublicBroadcasterCandidate,
    fee_amount: U256,
    gas_limit: u64,
    min_gas_price: u128,
}

impl ExecutorRecoveryFeeEstimate {
    #[must_use]
    pub const fn broadcaster(&self) -> &PublicBroadcasterCandidate {
        &self.broadcaster
    }

    #[must_use]
    pub const fn fee_amount(&self) -> U256 {
        self.fee_amount
    }

    #[must_use]
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    #[must_use]
    pub const fn min_gas_price(&self) -> u128 {
        self.min_gas_price
    }
}

pub struct ExecutorPaidRecoveryRequest {
    pub recovery: Arc<PreparedExecutorRecovery>,
    pub session: Arc<WalletSession>,
    pub authorization: DesktopPrivateSpendAuthorization,
    pub waku: Arc<WakuClient>,
    pub verify_proof: bool,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
}

/// A broadcaster response is transport status. Canonical execution and receipt
/// of the private shield output remain observable through the executor owner.
pub struct ExecutorPaidRecoveryOutcome {
    pub operation: ExecutorOperationId,
    pub payload_hash: B256,
    pub fee_token: Address,
    pub fee_amount: U256,
    pub gas_limit: u64,
    pub result: PublicBroadcasterResultKind,
}

impl ExecutorOwner {
    /// Preview the private fee without deriving keys, reserving an executor, or signing.
    /// Uses the submission planner and a conservative recovery budget including approval reset.
    pub async fn estimate_recovery_fee(
        &self,
        operation: ExecutorOperationId,
        asset: ExecutorAsset,
        session: &WalletSession,
        candidate: PublicBroadcasterCandidate,
    ) -> Result<ExecutorRecoveryFeeEstimate> {
        self.ensure_active()?;
        if !session
            .executor_owner()
            .is_some_and(|owner| std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!(
                "recovery private fee wallet belongs to another session"
            ));
        }
        let record = self.recovery_record(operation)?;
        let profile =
            crate::settings::ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
                .ok_or_else(|| {
                    eyre!("this historical delegate does not support broadcaster recovery")
                })?;
        ExecutorDelivery::PublicBroadcaster(Box::new(candidate.clone())).admit(profile)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let pool = query_rpc_pool_with_http_client(chain.rpc_urls, &self.http);
        let min_gas_price = self
            .while_active(buffered_gas_price_from_rpc_pool(&pool, &chain.gas))
            .await?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
            relay_adapt_contract: chain.relay_adapt_contract,
        };
        let recovery_gas = maximum_recovery_gas_limit(asset, chain.gas.gas_limit_buffer);
        // Only the private fee affects note selection. The recovery calls' gas is
        // accounted for above; their signed calldata is created after authorization.
        let request = MixedPrivateActionRequest {
            executor: None,
            executor_calls: Vec::new(),
            private_sends: vec![MixedPrivateSend {
                token_address: candidate.token,
                amount: U256::ONE,
                recipient: candidate.address_data,
                role: MixedPrivateSendRole::Other,
            }],
            public_unshields: Vec::new(),
            relay_actions: None,
            min_gas_price: public_broadcaster_bound_min_gas_price(
                self.chain.chain_id,
                min_gas_price,
            ),
            verify_proof: false,
            spend_up_to: false,
            rebuild: None,
        };
        let (gas_limit, fee_amount) = estimate_recovery_fee(
            &builder,
            &session.unspent_utxos(),
            request,
            recovery_gas,
            candidate.fee,
            U256::MAX,
            min_gas_price,
        )?;
        self.ensure_active()?;
        Ok(ExecutorRecoveryFeeEstimate {
            broadcaster: candidate,
            fee_amount,
            gas_limit,
            min_gas_price,
        })
    }

    /// Pay a compatible broadcaster from private notes while shielding the exact
    /// reviewed asset from its historical executor. Closing the owner cancels work;
    /// any already issued payload and its input reservations remain durable.
    pub async fn submit_paid_recovery(
        &self,
        request: ExecutorPaidRecoveryRequest,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        self.while_active(Box::pin(self.submit_paid_recovery_active(request)))
            .await
    }

    async fn submit_paid_recovery_active(
        &self,
        request: ExecutorPaidRecoveryRequest,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        if !request
            .session
            .executor_owner()
            .is_some_and(|owner| std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!(
                "recovery private fee wallet belongs to another session"
            ));
        }
        let preparation =
            self.prepare_broadcaster_recovery_execution(Arc::clone(&request.recovery))?;
        let ExecutorRecoveryFunding::PublicBroadcaster {
            candidate,
            maximum_private_fee,
        } = request.recovery.funding()
        else {
            return Err(eyre!("this recovery selected another funding route"));
        };
        preparation.require_broadcaster(candidate)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, &self.http);
        let min_gas_price = buffered_gas_price_from_rpc_pool(&query_rpc_pool, &chain.gas).await?;
        let bound_min_gas_price =
            public_broadcaster_bound_min_gas_price(self.chain.chain_id, min_gas_price);
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
            relay_adapt_contract: chain.relay_adapt_contract,
        };
        update_transaction_generation_stage(
            request.progress_tx.as_ref(),
            TransactionGenerationStage::SelectingPrivateNotes,
        );
        let utxos = request.session.unspent_utxos_for_executor(&preparation)?;
        let (_, mut fee_amount) = estimate_recovery_fee(
            &builder,
            &utxos,
            recovery_fee_request(
                &preparation,
                &request.recovery,
                candidate,
                U256::ONE,
                bound_min_gas_price,
                request.verify_proof,
            ),
            request.recovery.gas_limits()[0],
            candidate.fee,
            *maximum_private_fee,
            min_gas_price,
        )?;
        let source = artifact_source(&self.http, &request.session.db)?;
        let prover = ProverService::new_with_db(&source, &request.session.db);
        let chain_handle = request
            .session
            .sync_manager
            .chain_handle(&request.session.chain_key)
            .await
            .ok_or_else(|| eyre!("recovery private fee chain is unavailable"))?;
        let mut forest = chain_handle.forest.read().await.clone();
        forest.compute_roots();
        let signer = request.authorization.signer(
            &self.vault,
            &self.view,
            "executor recovery private fee",
        )?;

        for _ in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
            require_recovery_fee_limit(fee_amount, *maximum_private_fee)?;
            self.validate_preparation(&preparation)?;
            preparation.require_broadcaster(candidate)?;
            let utxos = request.session.unspent_utxos_for_executor(&preparation)?;
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::ProvingTransaction,
            );
            let plan = builder
                .build_mixed_private_action_plan_with_signer(
                    &self.view.scan_keys(),
                    &signer,
                    &forest,
                    &utxos,
                    recovery_fee_request(
                        &preparation,
                        &request.recovery,
                        candidate,
                        fee_amount,
                        bound_min_gas_price,
                        request.verify_proof,
                    ),
                    &prover,
                )
                .await
                .map_err(recovery_fee_build_error)?;
            let (mut grant, seed) = request.authorization.executor_spend_grant(&self.vault)?;
            let inputs = plan
                .inputs
                .iter()
                .map(|input| input.utxo.clone())
                .collect::<Vec<_>>();
            // Even simulation exposes a usable signed payload. Persist it first.
            let issued = self
                .issue_operation(&preparation, &plan.call, &inputs, &mut grant, seed)
                .await?;
            let mut transaction = issued.transaction().clone();
            if transaction.transaction_type == Some(4) {
                transaction.max_fee_per_gas =
                    Some(public_broadcaster_service_gas_price(min_gas_price));
                transaction.max_priority_fee_per_gas = Some(min_gas_price);
            }
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::EstimatingBroadcasterFee,
            );
            let (gas_limit, required_fee) = estimate_public_broadcaster_fee_from_rpc_pool(
                &query_rpc_pool,
                self.chain.chain_id,
                transaction.clone(),
                candidate.fee,
                min_gas_price,
                chain.gas.gas_limit_buffer,
            )
            .await?;
            if required_fee > fee_amount {
                fee_amount = buffered_public_broadcaster_fee(required_fee);
                require_recovery_fee_limit(fee_amount, *maximum_private_fee)?;
                continue;
            }
            transaction.gas = Some(gas_limit);
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::GeneratingPoiProofs,
            );
            let pois = public_broadcaster_pre_transaction_pois(
                &plan.chunks,
                candidate,
                &request.session,
                self.chain.chain_id,
                &prover,
                request.verify_proof,
                &self.http,
            )
            .await?;
            // The fee recipient belongs to the broadcaster. Only this wallet's
            // change receives pending output POI context; the shield is indexed normally.
            let change = plan
                .private_outputs
                .into_iter()
                .filter(|output| output.role == MixedPrivateOutputRole::Change)
                .collect::<Vec<_>>();
            persist_pending_mixed_output_poi_contexts(
                &request.session,
                &plan.chunks,
                &change,
                &pois.pending_pois,
                &pois.pending_poi_list_keys,
            )
            .await?;
            self.validate_preparation(&preparation)?;
            preparation.require_broadcaster(candidate)?;
            let result = submit_public_broadcaster_transaction(
                Arc::clone(&request.waku),
                transaction,
                pois.request_pois,
                candidate,
                bound_min_gas_price,
                request.progress_tx.clone(),
                request.response_timeout,
                request.republish_interval,
            )
            .await?;
            if let PublicBroadcasterResultKind::Submitted { tx_hash } = &result {
                self.store.record_submission(
                    issued.operation(),
                    issued.payload_hash(),
                    tx_hash.parse()?,
                )?;
                self.notify_change();
            }
            return Ok(ExecutorPaidRecoveryOutcome {
                operation: issued.operation(),
                payload_hash: issued.payload_hash(),
                fee_token: candidate.token,
                fee_amount,
                gas_limit,
                result,
            });
        }
        Err(eyre!(
            "recovery private fee did not stabilize; refresh the fee quote and review again"
        ))
    }
}

fn recovery_fee_request(
    preparation: &PreparedExecutorOperation,
    recovery: &PreparedExecutorRecovery,
    candidate: &PublicBroadcasterCandidate,
    fee_amount: U256,
    min_gas_price: u128,
    verify_proof: bool,
) -> MixedPrivateActionRequest {
    MixedPrivateActionRequest {
        executor: Some(preparation.context()),
        executor_calls: recovery.calls().to_vec(),
        private_sends: vec![MixedPrivateSend {
            token_address: candidate.token,
            amount: fee_amount,
            recipient: candidate.address_data,
            role: MixedPrivateSendRole::Other,
        }],
        public_unshields: Vec::new(),
        relay_actions: None,
        min_gas_price,
        verify_proof,
        spend_up_to: false,
        rebuild: None,
    }
}

fn estimate_recovery_fee(
    builder: &TransactionBuilder,
    utxos: &[Utxo],
    mut request: MixedPrivateActionRequest,
    recovery_gas: u64,
    fee_per_unit_gas: U256,
    maximum_private_fee: U256,
    min_gas_price: u128,
) -> Result<(u64, U256)> {
    // This request contains only the broadcaster payment; recovered assets stay in its calls.
    let mut fee = U256::ONE;
    for _ in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        require_recovery_fee_limit(fee, maximum_private_fee)?;
        request.private_sends[0].amount = fee;
        let preview = builder
            .preview_mixed_private_action_plan(utxos, &request)
            .map_err(recovery_fee_build_error)?;
        let gas = approximate_public_broadcaster_gas(ApproximateTransactionShape {
            transaction_count: preview.shape.transaction_count,
            input_count: preview.shape.input_count,
            private_output_count: preview.shape.private_output_count,
            public_output_count: 0,
            max_receiver_amount: fee,
            relay_call_count: preview.shape.relay_call_count,
            uses_relay_adapt: true,
            unwrap_count: 0,
            send: true,
            // The reviewed recovery batch budget already includes delegation/signature overhead.
            executor: false,
        })
        .saturating_add(recovery_gas);
        let required = buffered_public_broadcaster_fee(broadcaster_fee_amount(
            fee_per_unit_gas,
            gas,
            public_broadcaster_service_gas_price(min_gas_price),
        ));
        if fee >= required {
            return Ok((gas, fee));
        }
        fee = required;
    }
    Err(eyre!(
        "recovery fee estimate did not stabilize; refresh the fee quote"
    ))
}

fn require_recovery_fee_limit(fee: U256, maximum: U256) -> Result<()> {
    if fee > maximum {
        return Err(eyre!(
            "recovery private fee exceeds the reviewed maximum; review a new fee limit"
        ));
    }
    Ok(())
}

fn recovery_fee_build_error(error: BuildError) -> eyre::Report {
    if matches!(
        error,
        BuildError::InsufficientBalance(_) | BuildError::InsufficientFeeTokenBalance(_)
    ) {
        return eyre::Report::new(error).wrap_err("add POI-spendable private funds in the selected fee token, or fund the executor's native gas and review that route");
    }
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use broadcaster_core::contracts::railgun::Call;
    use broadcaster_core::crypto::railgun::AddressData;
    use railgun_wallet::tx::ExecutorContext;
    use railgun_wallet::{Note, UtxoCommitmentKind, UtxoSource};

    #[test]
    fn executor_recovery_fee_quote_uses_private_fee_notes_within_reviewed_limit() {
        let fee_token = Address::repeat_byte(1);
        let wrapped_native = Address::repeat_byte(2);
        let executor = Address::repeat_byte(3);
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: 1,
            railgun_contract: Address::repeat_byte(4),
            relay_adapt_contract: Address::repeat_byte(5),
        };
        let request = MixedPrivateActionRequest {
            executor: Some(ExecutorContext {
                chain_id: 1,
                executor,
                delegate: alloy::primitives::address!("05ae73c5925d843864ae6f261f3175de2ebcd963"),
                execution_nonce: U256::ZERO,
            }),
            // Fee selection previews the same native-wrap action regardless of its value.
            executor_calls: vec![Call {
                to: wrapped_native,
                value: U256::from(10).pow(U256::from(18)),
                data: alloy::primitives::bytes!("d0e30db0"),
            }],
            private_sends: vec![MixedPrivateSend {
                token_address: fee_token,
                amount: U256::ONE,
                recipient: AddressData {
                    master_public_key: U256::ONE,
                    viewing_public_key: [1; 32],
                },
                role: MixedPrivateSendRole::Other,
            }],
            public_unshields: Vec::new(),
            relay_actions: None,
            min_gas_price: 1,
            verify_proof: false,
            spend_up_to: false,
            rebuild: None,
        };
        let note = |token| {
            Utxo::new(
                Note::new_change(U256::ONE, token, U256::from(100_000_000), [1; 16]),
                0,
                0,
                UtxoSource {
                    tx_hash: B256::repeat_byte(6),
                    block_number: 1,
                    block_timestamp: 1,
                },
                UtxoCommitmentKind::Transact,
            )
        };
        let rate = U256::from(10).pow(U256::from(18));
        let quote = |utxos: &[Utxo], recovery_gas, maximum| {
            estimate_recovery_fee(
                &builder,
                utxos,
                request.clone(),
                recovery_gas,
                rate,
                maximum,
                1,
            )
            .map(|(_, fee)| fee)
        };
        // A wallet holding only the recovered asset cannot pay a distinct private fee token.
        let error = quote(&[note(wrapped_native)], 300_000, U256::MAX).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<BuildError>(),
            Some(BuildError::InsufficientBalance(_))
        ));
        let funded = [note(fee_token)];
        let fee = quote(&funded, 300_000, U256::MAX).unwrap();
        // The read-only preview must price the same private notes before account
        // keys and the shield calls have been prepared with spend authorization.
        let mut preview = request.clone();
        preview.executor = None;
        preview.executor_calls.clear();
        let (_, preview_fee) =
            estimate_recovery_fee(&builder, &funded, preview, 300_000, rate, U256::MAX, 1).unwrap();
        assert_eq!(preview_fee, fee);
        assert!(fee > quote(&funded, 0, U256::MAX).unwrap());
        assert_eq!(quote(&funded, 300_000, fee).unwrap(), fee);
        assert!(quote(&funded, 300_000, fee - U256::ONE).is_err());
    }
}
