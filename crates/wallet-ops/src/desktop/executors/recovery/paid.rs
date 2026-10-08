use std::borrow::Borrow;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use broadcaster_core::contracts::railgun::Call;
use eyre::{Result, eyre};
use railgun_wallet::tx::{
    BuildError, GasEstimateMode, MixedPrivateActionRequest, MixedPrivateOutputRole,
    MixedPrivateSend, MixedPrivateSendRole, RailgunGasModel,
};
use railgun_wallet::{ProverService, TransactionBuilder, Utxo};
use tracing::Instrument as _;

use super::{
    ExecutorOwner, ExecutorRecoveryFunding, PreparedExecutorRecovery, maximum_recovery_gas_limit,
};
use crate::desktop::executor_observation::trace_step;
use crate::desktop::executors::IssueRetry;
use crate::desktop::executors::swap::swap_recovery_call_bound;
use crate::desktop::{
    ApproximateTransactionShape, PUBLIC_BROADCASTER_FEE_ATTEMPTS,
    approximate_public_broadcaster_gas, artifact_source, bounded_public_broadcaster_fee,
    buffered_gas_price_from_rpc_pool, effective_desktop_chain_config,
    estimate_public_broadcaster_fee_from_rpc_pool, query_rpc_pool_with_http_client,
    submit_public_broadcaster_transaction, update_transaction_generation_stage,
};
use crate::poi_contexts::{
    persist_pending_mixed_output_poi_contexts, public_broadcaster_pre_transaction_pois,
};
use crate::settings::ExecutorProfile;
use crate::vault::ExecutorOperationId;
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorDelivery, PreparedExecutorOperation,
    PublicBroadcasterCandidate, PublicBroadcasterResultKind, TransactionGenerationProgressSender,
    TransactionGenerationStage, WakuClient, WalletSession, broadcaster_fee_amount,
    public_broadcaster_bound_min_gas_price, public_broadcaster_service_gas_price,
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

/// A private broadcaster fee that requires a new spending approval.
#[derive(Debug, thiserror::Error)]
#[error("{purpose} private fee exceeds the reviewed maximum; review a new fee limit")]
pub struct ExecutorPrivateFeeLimitExceeded {
    purpose: &'static str,
    fee_token: Address,
    maximum: U256,
    required: U256,
}

impl ExecutorPrivateFeeLimitExceeded {
    pub(in crate::desktop::executors) const fn new(
        purpose: PaidExecutionPurpose,
        fee_token: Address,
        maximum: U256,
        required: U256,
    ) -> Self {
        Self {
            purpose: purpose.label(),
            fee_token,
            maximum,
            required,
        }
    }

    #[must_use]
    pub const fn fee_token(&self) -> Address {
        self.fee_token
    }

    #[must_use]
    pub const fn maximum(&self) -> U256 {
        self.maximum
    }

    #[must_use]
    pub const fn required(&self) -> U256 {
        self.required
    }
}

/// Broadcaster-paid executor executions share fee selection, issuance, POI, and
/// submission. The purpose names the spend approval and user-facing fee errors.
#[derive(Clone, Copy)]
pub(in crate::desktop::executors) enum PaidExecutionPurpose {
    Recovery,
    SwapSetup,
}

impl PaidExecutionPurpose {
    const fn label(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::SwapSetup => "swap setup",
        }
    }

    const fn signer_operation(self) -> &'static str {
        match self {
            Self::Recovery => "executor recovery private fee",
            Self::SwapSetup => "swap setup private fee",
        }
    }
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
        self.require_fee_session(session, PaidExecutionPurpose::Recovery)?;
        let record = self.recovery_record(operation)?;
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| {
                eyre!("this historical delegate does not support broadcaster recovery")
            })?;
        let swap_calls = swap_recovery_call_bound(&record);
        self.estimate_paid_execution_fee(
            PaidExecutionPurpose::Recovery,
            profile,
            candidate,
            &session.unspent_utxos(),
            |buffer| {
                maximum_recovery_gas_limit(
                    RailgunGasModel::for_chain(self.chain.chain_id),
                    asset,
                    swap_calls,
                    buffer,
                )
            },
        )
        .await
    }

    /// Preview a paid execute's private fee. `budget_gas` receives the chain's gas
    /// limit buffer and must cover the execution's calls, delegation overhead, and that
    /// buffer at least once.
    pub(in crate::desktop::executors) async fn estimate_paid_execution_fee(
        &self,
        purpose: PaidExecutionPurpose,
        profile: ExecutorProfile,
        candidate: PublicBroadcasterCandidate,
        inputs: &[Utxo],
        budget_gas: impl FnOnce(u64) -> u64,
    ) -> Result<ExecutorRecoveryFeeEstimate> {
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
        let budget_gas = budget_gas(chain.gas.gas_limit_buffer);
        // Only the private fee affects note selection. The calls' gas is accounted
        // for above; their signed calldata is created after authorization.
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
        let (gas_limit, fee_amount) = estimate_private_fee(
            &builder,
            inputs,
            request,
            budget_gas,
            candidate.fee,
            U256::MAX,
            min_gas_price,
            purpose,
        )?;
        self.ensure_active()?;
        Ok(ExecutorRecoveryFeeEstimate {
            broadcaster: candidate,
            fee_amount,
            gas_limit,
            min_gas_price,
        })
    }

    pub(in crate::desktop::executors) fn require_fee_session(
        &self,
        session: &WalletSession,
        purpose: PaidExecutionPurpose,
    ) -> Result<()> {
        if !session
            .executor_owner()
            .is_some_and(|owner| std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!(
                "{} private fee wallet belongs to another session",
                purpose.label()
            ));
        }
        Ok(())
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
        self.require_fee_session(&request.session, PaidExecutionPurpose::Recovery)?;
        let preparation =
            self.prepare_broadcaster_recovery_execution(Arc::clone(&request.recovery))?;
        let ExecutorRecoveryFunding::PublicBroadcaster {
            candidate,
            maximum_private_fee,
        } = request.recovery.funding()
        else {
            return Err(eyre!("this recovery selected another funding route"));
        };
        self.submit_paid_execution(
            PaidExecutionPurpose::Recovery,
            &preparation,
            candidate,
            request.recovery.calls(),
            request.recovery.gas_limits()[0],
            *maximum_private_fee,
            &request.session,
            request.authorization,
            &request.waku,
            request.verify_proof,
            request.progress_tx.as_ref(),
            request.response_timeout,
            request.republish_interval,
        )
        .await
    }

    /// Prove, issue, and hand one executor `execute` to its selected broadcaster.
    /// The only private output is the broadcaster fee; `calls` run as its actions.
    pub(in crate::desktop::executors) async fn submit_paid_execution<
        A: Borrow<DesktopPrivateSpendAuthorization> + Send,
    >(
        &self,
        purpose: PaidExecutionPurpose,
        preparation: &PreparedExecutorOperation,
        candidate: &PublicBroadcasterCandidate,
        calls: &[Call],
        budget_gas: u64,
        maximum_private_fee: U256,
        session: &WalletSession,
        authorization: A,
        waku: &Arc<WakuClient>,
        verify_proof: bool,
        progress_tx: Option<&TransactionGenerationProgressSender>,
        response_timeout: Duration,
        republish_interval: Duration,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        let span = tracing::debug_span!(target: "executor_observation", "paid_execution",
            purpose = purpose.label());
        trace_step(
            "paid_total",
            self.submit_paid_execution_active(
                purpose,
                preparation,
                candidate,
                calls,
                budget_gas,
                maximum_private_fee,
                session,
                authorization,
                waku,
                verify_proof,
                progress_tx,
                response_timeout,
                republish_interval,
            ),
        )
        .instrument(span)
        .await
    }

    async fn submit_paid_execution_active<A: Borrow<DesktopPrivateSpendAuthorization> + Send>(
        &self,
        purpose: PaidExecutionPurpose,
        preparation: &PreparedExecutorOperation,
        candidate: &PublicBroadcasterCandidate,
        calls: &[Call],
        budget_gas: u64,
        maximum_private_fee: U256,
        session: &WalletSession,
        authorization: A,
        waku: &Arc<WakuClient>,
        verify_proof: bool,
        progress_tx: Option<&TransactionGenerationProgressSender>,
        response_timeout: Duration,
        republish_interval: Duration,
    ) -> Result<ExecutorPaidRecoveryOutcome> {
        preparation.require_broadcaster(candidate)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, &self.http);
        let min_gas_price = trace_step(
            "paid_gas_price",
            buffered_gas_price_from_rpc_pool(&query_rpc_pool, &chain.gas),
        )
        .await?;
        let bound_min_gas_price =
            public_broadcaster_bound_min_gas_price(self.chain.chain_id, min_gas_price);
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
            relay_adapt_contract: chain.relay_adapt_contract,
        };
        update_transaction_generation_stage(
            progress_tx,
            TransactionGenerationStage::SelectingPrivateNotes,
        );
        let utxos = session.unspent_utxos_for_executor(preparation)?;
        let (_, mut fee_amount) = trace_step("paid_fee_quote", async {
            estimate_private_fee(
                &builder,
                &utxos,
                paid_execution_request(
                    preparation,
                    calls,
                    candidate,
                    U256::ONE,
                    bound_min_gas_price,
                    verify_proof,
                ),
                budget_gas,
                candidate.fee,
                maximum_private_fee,
                min_gas_price,
                purpose,
            )
        })
        .await?;
        let source = artifact_source(&self.http, &session.db)?;
        let prover = ProverService::new_with_db(&source, &session.db);
        let chain_handle = session
            .sync_manager
            .chain_handle(&session.chain_key)
            .await
            .ok_or_else(|| eyre!("{} private fee chain is unavailable", purpose.label()))?;
        let mut forest = trace_step("paid_forest", async {
            Ok::<_, eyre::Report>(chain_handle.forest.read().await.clone())
        })
        .await?;
        forest.compute_roots();
        let signer =
            authorization
                .borrow()
                .signer(&self.vault, &self.view, purpose.signer_operation())?;
        // Repriced rounds may reuse the first round's chain evidence. Dropped
        // before submission so a lost broadcaster response cannot enable reuse.
        let mut issue_retry = IssueRetry::default();

        for round in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
            let span = tracing::debug_span!(target: "executor_observation", "paid_round", round);
            require_private_fee_limit(candidate.token, fee_amount, maximum_private_fee, purpose)?;
            self.validate_preparation(preparation)?;
            preparation.require_broadcaster(candidate)?;
            let utxos = session.unspent_utxos_for_executor(preparation)?;
            update_transaction_generation_stage(
                progress_tx,
                TransactionGenerationStage::ProvingTransaction,
            );
            let plan = trace_step(
                "paid_transaction_proof",
                builder.build_mixed_private_action_plan_with_signer(
                    &self.view.scan_keys(),
                    &signer,
                    &forest,
                    &utxos,
                    paid_execution_request(
                        preparation,
                        calls,
                        candidate,
                        fee_amount,
                        bound_min_gas_price,
                        verify_proof,
                    ),
                    &prover,
                ),
            )
            .instrument(span.clone())
            .await
            .map_err(|error| private_fee_build_error(error, purpose))?;
            let inputs = plan
                .inputs
                .iter()
                .map(|input| input.utxo.clone())
                .collect::<Vec<_>>();
            // Even simulation exposes a usable signed payload. Persist it first.
            let issued = trace_step(
                "paid_issue",
                self.issue_operation_retrying(
                    preparation,
                    &plan.call,
                    &inputs,
                    authorization.borrow(),
                    &mut issue_retry,
                ),
            )
            .instrument(span.clone())
            .await?;
            let mut transaction = issued.transaction().clone();
            if transaction.transaction_type == Some(4) {
                transaction.max_fee_per_gas =
                    Some(public_broadcaster_service_gas_price(min_gas_price));
                transaction.max_priority_fee_per_gas = Some(min_gas_price);
            }
            update_transaction_generation_stage(
                progress_tx,
                TransactionGenerationStage::EstimatingBroadcasterFee,
            );
            let (gas_limit, required_fee) = trace_step(
                "paid_broadcaster_fee_estimate",
                estimate_public_broadcaster_fee_from_rpc_pool(
                    &query_rpc_pool,
                    self.chain.chain_id,
                    transaction.clone(),
                    candidate.fee,
                    min_gas_price,
                    chain.gas.gas_limit_buffer,
                ),
            )
            .instrument(span.clone())
            .await?;
            if required_fee > fee_amount {
                fee_amount = bounded_private_fee(
                    candidate.token,
                    required_fee,
                    maximum_private_fee,
                    purpose,
                )?;
                continue;
            }
            // Keep approval through fee retries. Owned authorization is released before POI
            // and submission; a borrowed one remains with its caller.
            drop(issue_retry);
            drop(signer);
            drop(authorization);
            transaction.gas = Some(gas_limit);
            update_transaction_generation_stage(
                progress_tx,
                TransactionGenerationStage::GeneratingPoiProofs,
            );
            let pois = trace_step(
                "paid_pre_transaction_poi",
                public_broadcaster_pre_transaction_pois(
                    &plan.chunks,
                    candidate,
                    session,
                    self.chain.chain_id,
                    &prover,
                    verify_proof,
                    &self.http,
                ),
            )
            .instrument(span.clone())
            .await?;
            // The fee recipient belongs to the broadcaster. Only this wallet's
            // change receives pending output POI context; the shield is indexed normally.
            let change = plan
                .private_outputs
                .into_iter()
                .filter(|output| output.role == MixedPrivateOutputRole::Change)
                .collect::<Vec<_>>();
            trace_step(
                "paid_poi_context_persist",
                persist_pending_mixed_output_poi_contexts(
                    session,
                    &plan.chunks,
                    &change,
                    &pois.pending_pois,
                    &pois.pending_poi_list_keys,
                ),
            )
            .instrument(span.clone())
            .await?;
            self.validate_preparation(preparation)?;
            preparation.require_broadcaster(candidate)?;
            let result = trace_step(
                "paid_submit",
                submit_public_broadcaster_transaction(
                    Arc::clone(waku),
                    transaction,
                    pois.request_pois,
                    candidate,
                    bound_min_gas_price,
                    progress_tx.cloned(),
                    response_timeout,
                    republish_interval,
                ),
            )
            .instrument(span.clone())
            .await?;
            if let PublicBroadcasterResultKind::Submitted { tx_hash } = &result {
                let transaction_hash: B256 = tx_hash.parse()?;
                trace_step("paid_record_submission", async {
                    self.store.record_submission(
                        issued.operation(),
                        issued.payload_hash(),
                        transaction_hash,
                    )
                })
                .instrument(span)
                .await?;
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
            "{} private fee did not stabilize; refresh the fee quote and review again",
            purpose.label()
        ))
    }
}

fn paid_execution_request(
    preparation: &PreparedExecutorOperation,
    calls: &[Call],
    candidate: &PublicBroadcasterCandidate,
    fee_amount: U256,
    min_gas_price: u128,
    verify_proof: bool,
) -> MixedPrivateActionRequest {
    MixedPrivateActionRequest {
        executor: Some(preparation.context()),
        executor_calls: calls.to_vec(),
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

fn estimate_private_fee(
    builder: &TransactionBuilder,
    utxos: &[Utxo],
    mut request: MixedPrivateActionRequest,
    budget_gas: u64,
    fee_per_unit_gas: U256,
    maximum_private_fee: U256,
    min_gas_price: u128,
    purpose: PaidExecutionPurpose,
) -> Result<(u64, U256)> {
    // This request contains only the broadcaster payment; executor actions stay in its calls.
    let model = RailgunGasModel::for_chain(builder.chain_id);
    let mut fee = U256::ONE;
    for _ in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        require_private_fee_limit(
            request.private_sends[0].token_address,
            fee,
            maximum_private_fee,
            purpose,
        )?;
        request.private_sends[0].amount = fee;
        let preview = builder
            .preview_mixed_private_action_plan(utxos, &request)
            .map_err(|error| private_fee_build_error(error, purpose))?;
        // `budget_gas` is the reviewed execution budget: the executor's calls, its delegation
        // and signature overhead, and the chain's gas limit buffer. Only the fee payment's
        // Railgun transaction is priced here, so the quote doesn't depend on the calls.
        let gas = approximate_public_broadcaster_gas(
            model,
            GasEstimateMode::Expected,
            ApproximateTransactionShape {
                transaction_count: preview.shape.transaction_count,
                input_count: preview.shape.input_count,
                private_output_count: preview.shape.private_output_count,
                public_output_count: 0,
                max_receiver_amount: fee,
                relay_call_count: 0,
                uses_relay_adapt: false,
                unwrap_count: 0,
                executor: false,
            },
        )
        .saturating_add(budget_gas);
        let required = bounded_private_fee(
            request.private_sends[0].token_address,
            broadcaster_fee_amount(
                fee_per_unit_gas,
                gas,
                public_broadcaster_service_gas_price(min_gas_price),
            ),
            maximum_private_fee,
            purpose,
        )?;
        if fee >= required {
            return Ok((gas, fee));
        }
        fee = required;
    }
    Err(eyre!(
        "{} fee estimate did not stabilize; refresh the fee quote",
        purpose.label()
    ))
}

fn bounded_private_fee(
    fee_token: Address,
    required: U256,
    maximum: U256,
    purpose: PaidExecutionPurpose,
) -> Result<U256> {
    // Preserve the amounts for review when the required fee itself exceeds approval.
    // Optional padding can consume the remaining allowance, but cannot raise it.
    require_private_fee_limit(fee_token, required, maximum, purpose)?;
    bounded_public_broadcaster_fee(required, Some(maximum))
}

pub(in crate::desktop::executors) fn require_private_fee_limit(
    fee_token: Address,
    fee: U256,
    maximum: U256,
    purpose: PaidExecutionPurpose,
) -> Result<()> {
    if fee > maximum {
        return Err(ExecutorPrivateFeeLimitExceeded::new(purpose, fee_token, maximum, fee).into());
    }
    Ok(())
}

fn private_fee_build_error(error: BuildError, purpose: PaidExecutionPurpose) -> eyre::Report {
    if matches!(
        error,
        BuildError::InsufficientBalance(_) | BuildError::InsufficientFeeTokenBalance(_)
    ) {
        return eyre::Report::new(error).wrap_err(match purpose {
            PaidExecutionPurpose::Recovery => {
                "add POI-spendable private funds in the selected fee token, or fund the executor's native gas and review that route"
            }
            PaidExecutionPurpose::SwapSetup => {
                "add POI-spendable private funds in the selected fee token"
            }
        });
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
            estimate_private_fee(
                &builder,
                utxos,
                request.clone(),
                recovery_gas,
                rate,
                maximum,
                1,
                PaidExecutionPurpose::Recovery,
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
        let (gas, preview_fee) = estimate_private_fee(
            &builder,
            &funded,
            preview,
            300_000,
            rate,
            U256::MAX,
            1,
            PaidExecutionPurpose::Recovery,
        )
        .unwrap();
        assert_eq!(preview_fee, fee);
        assert!(fee > quote(&funded, 0, U256::MAX).unwrap());
        assert_eq!(quote(&funded, 300_000, fee).unwrap(), fee);
        // Approval headroom must not become part of the private fee payment.
        let maximum = crate::default_public_broadcaster_fee_limit(fee);
        assert_eq!(quote(&funded, 300_000, maximum).unwrap(), fee);
        let required = broadcaster_fee_amount(rate, gas, public_broadcaster_service_gas_price(1));
        let approved = quote(&funded, 299_000, U256::MAX).unwrap();
        // A higher execution estimate can consume the earlier quote's cushion.
        assert!(required < approved && approved < fee);
        assert_eq!(quote(&funded, 300_000, approved).unwrap(), approved);
        // The entire cushion is optional, but the unbuffered fee must still fit.
        assert_eq!(quote(&funded, 300_000, required).unwrap(), required);
        let maximum = required - U256::ONE;
        let error = quote(&funded, 300_000, maximum).unwrap_err();
        let exceeded = error
            .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
            .unwrap();
        assert_eq!(exceeded.fee_token(), fee_token);
        assert_eq!(exceeded.maximum(), maximum);
        assert_eq!(exceeded.required(), required);
    }
}
