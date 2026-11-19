#[allow(clippy::wildcard_imports)]
use super::*;

impl ExecutorOwner {
    /// Sign the final proved call under current spend authorization. The shared
    /// context checks proof binding and hashes all transactions and action data.
    pub async fn issue_operation(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
        client: &OperationHttpClient,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_operation_with(prepared, call, inputs, authorization, None, client)
            .await
    }

    /// Issue one round of a paid fee-convergence loop. `retry` belongs to that
    /// loop; see [`IssueRetry`] for when a repriced round skips the chain reads.
    pub(crate) async fn issue_operation_retrying(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
        retry: &mut IssueRetry,
        client: &OperationHttpClient,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_operation_with(prepared, call, inputs, authorization, Some(retry), client)
            .await
    }

    async fn issue_operation_with(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
        retry: Option<&mut IssueRetry>,
        client: &OperationHttpClient,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_payload(prepared, authorization, retry, client, |_| {
            let hash = prepared.context.signing_hash(call)?;
            // The durable reservation must cover precisely the private inputs whose
            // nullifiers the signed execute can spend, including every inner transaction.
            let decoded = RelayAdapt7702::executeCall::abi_decode(&call.data)?;
            if let Some(recovery) = &prepared.recovery
                && (decoded._actionData.minGasLimit != U256::ZERO
                    || decoded._actionData.calls.len() != recovery.calls().len()
                    || decoded._actionData.calls.iter().zip(recovery.calls()).any(
                        |(actual, expected)| {
                            actual.to != expected.to
                                || actual.value != expected.value
                                || actual.data != expected.data
                        },
                    ))
            {
                return Err(eyre!(
                    "proved recovery calls differ from the reviewed actions"
                ));
            }
            let mut expected = decoded
                ._transactions
                .iter()
                .flat_map(|transaction| {
                    transaction.nullifiers.iter().map(|nullifier| {
                        (u32::from(transaction.boundParams.treeNumber), *nullifier)
                    })
                })
                .collect::<Vec<_>>();
            let nullifying_key = self.view.scan_keys().nullifying_key;
            let mut supplied = inputs
                .iter()
                .map(|input| (input.tree, B256::from(input.nullifier(nullifying_key))))
                .collect::<Vec<_>>();
            expected.sort_unstable();
            supplied.sort_unstable();
            if expected != supplied || supplied.is_empty() {
                return Err(eyre!(
                    "executor input reservations do not match the proved transaction"
                ));
            }
            if self.records()?.iter().any(|other| {
                other.operation() != prepared.operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|reserved| inputs.iter().any(|input| reserved.matches(input)))
            }) {
                return Err(eyre!(
                    "private inputs are reserved by another executor operation"
                ));
            }
            Ok(PayloadToSign {
                hash,
                value: U256::ZERO,
                inputs: inputs
                    .iter()
                    .map(ExecutorInputIdentity::from_utxo)
                    .collect(),
                authorize_delegation: false,
                authorize: Box::new(move |signature| {
                    Ok(prepared.context.authorize_call(call, signature)?)
                }),
            })
        })
        .await
    }

    /// Issue one execute payload under the shared executor admission, inspection,
    /// signing and persistence rules. `payload` runs under the activity lock right
    /// after the preparation is validated and before any chain read.
    pub(in crate::desktop::executors) async fn issue_payload<'a>(
        &self,
        prepared: &PreparedExecutorOperation,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        mut retry: Option<&mut IssueRetry>,
        client: &OperationHttpClient,
        payload: impl FnOnce(&crate::vault::ExecutorRecord) -> Result<PayloadToSign<'a>>,
    ) -> Result<IssuedExecutorTransaction> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self.validate_preparation(prepared)?;
        let chain = if prepared.recovery.is_some() {
            let mut chain = self
                .chain_for_delegate(record.delegate())
                .ok_or_else(|| eyre!("chain does not support Railgun"))?;
            chain.enabled = true;
            chain
        } else {
            self.chain.clone()
        };
        let chain = Box::new(chain);
        let profile = ExecutorProfile::accepted(chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let PayloadToSign {
            hash,
            value,
            inputs,
            authorize_delegation: require_authorization,
            authorize,
        } = payload(&record)?;
        let initially_unissued = prepared.recovery.is_none() && record.issued().is_empty();
        let reused = match retry.as_deref_mut() {
            Some(retry) if !initially_unissued => {
                let reused = retry.reusable(prepared, &record);
                if reused.is_none() {
                    // Once this loop needs fresh evidence, later rounds need it too.
                    retry.seed = None;
                }
                reused
            }
            _ => None,
        };
        let checked = if let Some(checked) = &reused {
            Arc::clone(checked)
        } else if initially_unissued {
            self.unused_inspection(
                record.index(),
                prepared.context.executor,
                &prepared.inspection_assets,
                client,
            )
            .await?
        } else {
            let (inspection, observed) = if let Some(recovery) = &prepared.recovery {
                // Keep this larger branch out of the setup/signing future, including
                // operations that never take the recovery path.
                self.while_active(Box::pin(
                    crate::desktop::executor_discovery::inspect_for_recovery_batch(
                        &chain,
                        &self.http,
                        client,
                        prepared.context.executor,
                        &[recovery.asset()],
                    ),
                ))
                .await?
            } else {
                self.while_active(inspect_for_signing(
                    &chain,
                    &self.http,
                    client,
                    prepared.context.executor,
                    &[],
                ))
                .await?
            };
            std::sync::Arc::new(crate::desktop::executors::spare::CheckedExecutor {
                inspection,
                observed,
                revision: None,
            })
        };
        let inspection = &checked.inspection;
        let observed = checked.observed;
        checked.ensure_valid()?;
        if observed.nonce() != prepared.context.execution_nonce {
            return Err(eyre!(
                "executor nonce changed; rebuild the approved operation"
            ));
        }
        if initially_unissued
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || !observed.nonce().is_zero())
        {
            self.store.retire(prepared.operation)?;
            return Err(ExecutorAccountUsed {
                operation: prepared.operation,
            }
            .into());
        }
        if let Some(recovery) = &prepared.recovery {
            crate::desktop::executors::recovery::recovery_funding_admission(
                inspection,
                recovery.asset(),
                recovery.amount(),
                recovery.funding(),
                recovery.gas_limits(),
            )?;
            recovery.validate_inspection(inspection)?;
        }
        // A retry with issued payloads reads the account again at the inspection's block.
        // An unresolved earlier nonce keeps future-nonce signing blocked. A reused round
        // keeps the durable observation its evidence matched.
        if reused.is_none() {
            let reconciled = self
                .admit_signing_read(&record, &chain, observed, client)
                .await?;
            if prepared.recovery.is_none() {
                require_unfinished_operation(&reconciled)?;
            }
        }
        let action = if let Some(recovery) = &prepared.recovery {
            self.recovery_authorization_action(authorization, recovery)?
        } else {
            crate::desktop::executors::HardwareExecutorAction::Execute(record.operation())
        };
        let signer = self.authorized_executor_signer(
            authorization,
            &action,
            record.operation(),
            record.index(),
        )?;
        if signer.address() != prepared.context.executor {
            return Err(eyre!(
                "executor signing identity does not match preparation"
            ));
        }
        self.ensure_active()?;
        let signed_call = authorize(signer.sign_hash_sync(&hash)?)?;
        let mut transaction = TransactionRequest::default()
            .to(signed_call.to)
            .input(signed_call.data.clone().into())
            .value(value);
        transaction.chain_id = Some(profile.chain_id());
        transaction.from = prepared.delivery.sender();
        authorize_delegation(
            &mut transaction,
            profile,
            inspection,
            &signer,
            prepared.recovery.is_some(),
            matches!(prepared.delivery, ExecutorDelivery::PublicBroadcaster(_))
                || require_authorization,
        )?;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let payload_context = ExecutorPayloadContext::new(signed_call.data, observed, inputs);
        let payload = IssuedExecutorPayload::new(
            observed.nonce(),
            profile.delegate(),
            hash,
            if prepared.recovery.is_some() {
                ExecutorPayloadPurpose::Recovery
            } else {
                ExecutorPayloadPurpose::Operation
            },
            payload_context,
        );
        if let Some(recovery) = &prepared.recovery {
            self.store.record_recovery_issued(
                prepared.operation,
                recovery.expected_active_use,
                payload,
            )?;
        } else {
            self.store.record_issued(prepared.operation, payload)?;
        }
        self.unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?
            .remove(record.index());
        if let Some(retry) = retry {
            if initially_unissued {
                retry.seed = Some(IssueRetrySeed {
                    operation: prepared.operation,
                    execution_nonce: prepared.context.execution_nonce,
                    checked: Arc::clone(&checked),
                    issued: BTreeSet::from([hash]),
                });
            } else if reused.is_some()
                && let Some(seed) = &mut retry.seed
            {
                seed.issued.insert(hash);
            }
        }
        self.ensure_active()?;
        self.notify_change();
        Ok(IssuedExecutorTransaction {
            operation: prepared.operation,
            payload_hash: hash,
            transaction,
        })
    }
}
