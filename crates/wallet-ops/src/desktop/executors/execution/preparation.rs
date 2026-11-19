#[allow(clippy::wildcard_imports)]
use super::*;

impl ExecutorOwner {
    /// Prepare with the same software authorization used by the private spend.
    pub async fn prepare_authorized_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
        client: &OperationHttpClient,
    ) -> Result<PreparedExecutorOperation> {
        self.prepare_operation(
            operation,
            delivery,
            authorization,
            assets,
            purpose_summary,
            client,
        )
        .await
    }

    /// Recheck native ownership before quoting or proving an existing preparation.
    pub fn validate_preparation(
        &self,
        prepared: &PreparedExecutorOperation,
    ) -> Result<crate::vault::ExecutorRecord> {
        self.ensure_active()?;
        if let Some(recovery) = &prepared.recovery {
            let record = self.validate_recovery(recovery)?;
            if !self.closed.same_channel(&prepared.owner)
                || prepared.generation != self.generation
                || prepared.wallet_id != self.view.wallet_id()
                || prepared.operation != record.operation()
                || prepared.context.executor != recovery.source()
                || prepared.context.chain_id != self.chain.chain_id
                || prepared.context.delegate != record.delegate()
                || recovery.execution()
                    != (ExecutorRecoveryExecution::PaidExecute {
                        nonce: prepared.context.execution_nonce,
                    })
            {
                return Err(eyre!(
                    "recovery execution preparation no longer matches its owner"
                ));
            }
            return Ok(record);
        }
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let profile = prepared.delivery.admit(profile)?;
        if !self.closed.same_channel(&prepared.owner)
            || prepared.generation != self.generation
            || prepared.wallet_id != self.view.wallet_id()
            || prepared.context.chain_id != profile.chain_id()
            || prepared.context.delegate != profile.delegate()
        {
            return Err(eyre!(
                "executor preparation belongs to a different wallet session"
            ));
        }
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == prepared.operation)
            .ok_or_else(|| eyre!("executor reservation is unavailable"))?;
        if record.is_retired()
            || record.address() != Some(prepared.context.executor)
            || record.delegate() != prepared.context.delegate
        {
            return Err(eyre!(
                "executor preparation no longer matches its reservation"
            ));
        }
        require_unfinished_operation(&record)?;
        Ok(record)
    }

    /// Bind paid recovery to the retained identity without allocating or exposing
    /// another account. Proof and delivery code carry this same preparation.
    pub fn prepare_broadcaster_recovery_execution(
        &self,
        recovery: std::sync::Arc<PreparedExecutorRecovery>,
    ) -> Result<PreparedExecutorOperation> {
        let record = self.validate_recovery(&recovery)?;
        let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = recovery.funding()
        else {
            return Err(eyre!("this recovery selected another funding route"));
        };
        let ExecutorRecoveryExecution::PaidExecute { nonce } = recovery.execution() else {
            return Err(eyre!(
                "broadcaster recovery requires the current execute wrapper"
            ));
        };
        Ok(PreparedExecutorOperation {
            operation: record.operation(),
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: self.chain.chain_id,
                executor: recovery.source(),
                delegate: record.delegate(),
                execution_nonce: nonce,
            },
            delivery: ExecutorDelivery::PublicBroadcaster(candidate.clone()),
            inspection_assets: Vec::new(),
            recovery: Some(recovery),
        })
    }

    /// Reserve durably before deriving or querying the address. Failed and used
    /// reservations remain visible for recovery and never enter the free pool.
    pub async fn prepare_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
        client: &OperationHttpClient,
    ) -> Result<PreparedExecutorOperation> {
        self.prepare_reserved_operation(
            operation,
            delivery,
            authorization,
            assets,
            OperationReservation::Operation {
                purpose_summary,
                swap_approval: None,
                destination_operation: None,
            },
            client,
        )
        .await
    }

    fn reserve_operation(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        assets: &[ExecutorAsset],
        reservation: &OperationReservation<'_>,
    ) -> Result<crate::vault::ExecutorRecord> {
        Ok(match reservation {
            OperationReservation::Operation {
                purpose_summary,
                swap_approval,
                destination_operation,
            } => self.store.reserve_with_swap_approval(
                operation,
                delegate,
                *purpose_summary,
                assets,
                swap_approval.cloned(),
                *destination_operation,
            )?,
            OperationReservation::SwapDestination(destination) => self
                .store
                .reserve_swap_destination(operation, delegate, *destination)?,
        })
    }

    /// [`Self::prepare_operation`] with the reservation `reservation` describes. A reservation
    /// that already exists keeps its own approval.
    pub(in crate::desktop::executors) async fn prepare_reserved_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        reservation: OperationReservation<'_>,
        client: &OperationHttpClient,
    ) -> Result<PreparedExecutorOperation> {
        self.require_executor_authorization(
            authorization,
            &crate::desktop::executors::HardwareExecutorAction::Execute(operation),
        )?;
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "activity_lock", "started");
        let guard = self.lock_activity().await;
        tracing::info!(
            target: "executor_preparation",
            step = "activity_lock",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        self.ensure_active()?;
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let profile = delivery.admit(profile)?;
        if let ExecutorDelivery::SelfBroadcast { sender, .. } = &delivery
            && self
                .vault
                .list_public_accounts_for_session(&self.view, true)?
                .iter()
                .any(|account| {
                    account.address == *sender
                        && account.source == crate::vault::PublicAccountSource::HardwareDerived
                })
        {
            return Err(eyre!(
                "Hardware-native Public accounts cannot pay gas for executor actions. Select a software or imported payer, or a broadcaster."
            ));
        }
        if let (
            crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware),
            ExecutorDelivery::SelfBroadcast { sender, .. },
        ) = (authorization, &delivery)
        {
            hardware.require_gas_payer(*sender)?;
        }
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "reserve_and_derive", "started");
        let record = self.reserve_operation(operation, profile.delegate(), assets, &reservation)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        let spare = self
            .store
            .reserve_spare(profile.delegate())
            .inspect_err(|_| {
                tracing::debug!(target: LOG_TARGET,
                    "executor spare allocation deferred until the next authorized action"
                );
            })
            .ok();
        let spare_index = spare
            .as_ref()
            .and_then(|spare| spare.address().is_none().then_some(spare.index()));
        let (address, spare_address) = self.authorized_executor_addresses(
            authorization,
            operation,
            record.index(),
            spare_index,
        )?;
        self.ensure_active()?;
        self.store.bind_address(operation, address)?;
        if let Some((index, address)) = spare_index.zip(spare_address) {
            self.store.bind_spare_address(index, address)?;
        }
        self.notify_change();
        tracing::info!(
            target: "executor_preparation",
            step = "reserve_and_derive",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        drop(guard);
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "chain_inspection", "started");
        let result = if record.issued().is_empty() {
            self.unused_inspection(record.index(), address, assets, client)
                .await
        } else {
            self.while_active(inspect_for_signing(
                &self.chain,
                &self.http,
                client,
                address,
                assets,
            ))
            .await
            .map(|(inspection, observed)| {
                std::sync::Arc::new(crate::desktop::executors::spare::CheckedExecutor {
                    inspection,
                    observed,
                    revision: None,
                })
            })
        };
        tracing::info!(
            target: "executor_preparation",
            step = "chain_inspection",
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "finished"
        );
        let checked = result?;
        let inspection = &checked.inspection;
        let observed = checked.observed;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let record = self.reserve_operation(operation, profile.delegate(), assets, &reservation)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        if record.issued().is_empty()
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || observed.nonce() != U256::ZERO)
        {
            self.store.retire(operation)?;
            return Err(ExecutorAccountUsed { operation }.into());
        }
        Ok(PreparedExecutorOperation {
            operation,
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: profile.chain_id(),
                executor: address,
                delegate: profile.delegate(),
                execution_nonce: observed.nonce(),
            },
            delivery,
            inspection_assets: assets.to_vec(),
            recovery: None,
        })
    }
}
