use super::*;

impl ExecutorStore {
    /// Keep the last successful observation when a later check fails.
    pub(crate) fn record_use_check(
        &self,
        operation: ExecutorOperationId,
        observation: Option<ExecutorUseObservation>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if let Some(observation) = observation {
                record.use_check.observation = Some(observation);
            }
            record.use_check.unavailable = observation.is_none();
            Ok(())
        })
    }

    pub fn bind_address(
        &self,
        operation: ExecutorOperationId,
        address: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.address.is_some_and(|existing| existing != address) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            record.address = Some(address);
            Ok(())
        })
    }

    /// Presentation preference only; hiding cannot release or reallocate an account.
    pub fn set_hidden(
        &self,
        operation: ExecutorOperationId,
        hidden: bool,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.hidden = hidden;
            Ok(())
        })
    }

    pub fn retire(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.retired = true;
            Ok(())
        })
    }

    /// Persist before releasing signed data. A competing recovery retains both identities.
    pub fn record_issued(
        &self,
        operation: ExecutorOperationId,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_issued_for(operation, SwapUseExpectation::Unchecked, payload)
    }

    /// [`Self::record_issued`] for a destination stealth account's shield, which is signed for
    /// the swap use `use_id`. It is refused unless that use still claims the account, checked in
    /// the write that persists the payload.
    pub fn record_swap_destination_shield(
        &self,
        operation: ExecutorOperationId,
        use_id: SwapUseId,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if payload.purpose != ExecutorPayloadPurpose::SwapDestinationShield {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.record_issued_for(operation, SwapUseExpectation::Exact(Some(use_id)), payload)
    }

    /// Persist a reviewed recovery only while its captured swap claim still matches.
    pub(crate) fn record_recovery_issued(
        &self,
        operation: ExecutorOperationId,
        expected_active_use: Option<SwapUseId>,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if payload.purpose != ExecutorPayloadPurpose::Recovery {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.record_issued_for(
            operation,
            SwapUseExpectation::Exact(expected_active_use),
            payload,
        )
    }

    fn record_issued_for(
        &self,
        operation: ExecutorOperationId,
        expected_active_use: SwapUseExpectation,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let (purpose, hash) = (payload.purpose, payload.hash);
        self.update(operation, |record| {
            if !expected_active_use.matches(record.active_swap_use) {
                return Err(ExecutorStoreError::SwapUseActive);
            }
            if record.address.is_none() || payload.delegate != record.delegate {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // A destination shield belongs to the account's active destination use, whose
            // origin swap on the other chain must name this account for the same use. A swap
            // paid from a Public account has no origin record to name it.
            let shield_use = if purpose == ExecutorPayloadPurpose::SwapDestinationShield {
                let Some(SwapUseRecord {
                    id,
                    stopped: false,
                    role,
                    ..
                }) = record.active_use()
                else {
                    return Err(ExecutorStoreError::OperationMismatch);
                };
                match role {
                    SwapUseRole::Destination {
                        origin_chain,
                        origin_operation,
                        ..
                    } => {
                        if !self
                            .for_chain(*origin_chain)
                            .record(*origin_operation)?
                            .is_some_and(|origin| origin.links_swap_destination(*id, operation))
                        {
                            return Err(ExecutorStoreError::OperationMismatch);
                        }
                    }
                    SwapUseRole::PublicSourceDestination { .. } => {}
                    SwapUseRole::Source { .. } => {
                        return Err(ExecutorStoreError::OperationMismatch);
                    }
                }
                Some(*id)
            } else {
                None
            };
            // A setup belongs to the swap use that reserved its account fresh, until that use
            // is stopped. An account a later use claims is already set up.
            let setup_closed = record
                .active_use()
                .is_some_and(|swap_use| swap_use.is_stopped() || !swap_use.is_fresh());
            // Swap hooks are recorded only together with their order. A destination shield
            // belongs to a live destination account and spends no private inputs.
            if (record.public_account_uuid.is_some() || record.swap_setup_stopped || setup_closed)
                && payload.purpose == ExecutorPayloadPurpose::Operation
                || matches!(
                    payload.purpose,
                    ExecutorPayloadPurpose::SwapPreHook | ExecutorPayloadPurpose::SwapPostHook
                )
                || payload.purpose == ExecutorPayloadPurpose::SwapDestinationShield
                    && (record.retired
                        || record.swap_setup_stopped
                        || !payload.context.inputs.is_empty())
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // This check shares the record mutation lock with every store handle.
            // Concurrent proofs may have selected the same previously free notes.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| payload.context.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            // Recovery, including early cancellation, competes with this swap's own pre-hook. So
            // does a setup sent again while an order on its included setup is open: it runs at
            // the nonce below the pre-hook's and would spend the pre-hook's notes first.
            if matches!(
                payload.purpose,
                ExecutorPayloadPurpose::Recovery | ExecutorPayloadPurpose::Operation
            ) && record
                .swap_reserved_inputs()
                .iter()
                .any(|input| payload.context.inputs.contains(input))
            {
                return Err(ExecutorStoreError::InputReserved);
            }
            if record.nonce_observation != Some(payload.context.observed)
                || payload.context.observed.nonce != payload.nonce
                || payload.context.calldata.is_empty()
                || payload.purpose == ExecutorPayloadPurpose::Operation
                    && record.issued.iter().any(|issued| {
                        issued.nonce < payload.nonce && !record.nonce_resolved(issued.nonce)
                    })
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            if let Some(existing) = record
                .issued
                .iter()
                .find(|issued| issued.hash == payload.hash)
            {
                if existing.nonce != payload.nonce
                    || existing.delegate != payload.delegate
                    || existing.purpose != payload.purpose
                    || existing.context.calldata != payload.context.calldata
                    || existing.context.inputs != payload.context.inputs
                {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
                // Recording a released payload again means it is being sent again.
                record
                    .released_payloads
                    .retain(|hash| *hash != payload.hash);
            } else {
                if payload.purpose == ExecutorPayloadPurpose::Recovery {
                    record.retired = true;
                }
                record.issued.push(payload);
            }
            if let Some(id) = shield_use
                && let Some(SwapUseRecord {
                    role:
                        SwapUseRole::Destination {
                            shields, outcome, ..
                        }
                        | SwapUseRole::PublicSourceDestination {
                            shields, outcome, ..
                        },
                    ..
                }) = record.swap_use_mut(id)
            {
                if !shields.contains(&hash) {
                    shields.push(hash);
                }
                // A newly signed order's fill can fund the shield again.
                if *outcome == Some(SwapDestinationOutcome::Unfilled) {
                    *outcome = None;
                }
            }
            Ok(())
        })
    }
}
