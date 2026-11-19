use super::*;

impl ExecutorOwner {
    /// POI-spendable notes, without other operations' reservations or notes this swap's
    /// executor already spent.
    pub(super) fn swap_inputs(
        &self,
        session: &WalletSession,
        record: &ExecutorRecord,
    ) -> Result<Vec<Utxo>> {
        self.inputs_for_swap_order(self.spendable_swap_notes(session)?, record)
    }

    /// Notes of `inputs` an order of `record`'s swap may spend: [`Self::inputs_for_record`]
    /// without a note of a setup whose nonce is unresolved. Any setup attempt at that nonce can
    /// still run before the pre-hook, as it does below an order on an included setup, and
    /// spend the note as its fee.
    pub(crate) fn inputs_for_swap_order(
        &self,
        inputs: Vec<Utxo>,
        record: &ExecutorRecord,
    ) -> Result<Vec<Utxo>> {
        let mut inputs = self.inputs_for_record(inputs, record)?;
        inputs.retain(|input| {
            !record.issued().iter().any(|payload| {
                payload.purpose() == ExecutorPayloadPurpose::Operation
                    && !record.nonce_resolved(payload.nonce())
                    && payload
                        .context()
                        .inputs()
                        .iter()
                        .any(|spent| spent.matches(input))
            })
        });
        Ok(inputs)
    }

    /// Notes a setup of the swap `operation`, or of a new swap, may pay its fee with: as for
    /// any setup preparation, without the notes the account's open pre-hooks reserve, since a
    /// setup sent again runs before a pre-hook on its included setup. The fee preview selects
    /// as signing does.
    pub(in crate::desktop::executors::swap) fn swap_setup_fee_inputs(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
    ) -> Result<Vec<Utxo>> {
        let notes = self.spendable_swap_notes(session)?;
        match operation {
            Some(operation) => {
                let record = self
                    .swap_account_record(operation)?
                    .ok_or_else(|| eyre!("swap executor is unavailable"))?;
                self.inputs_for_recovery(notes, &record)
            }
            None => self.filter_reserved_inputs(notes, None),
        }
    }

    /// Whether the automatic attempt to place the approved order of the swap `operation`,
    /// spending `spend` of `token`, should quote now. One current snapshot of the wallet's
    /// notes answers both questions. The spend fits when it fits the notes planning would
    /// select, as [`Self::max_swap_amounts`] counts them. When it doesn't, the attempt waits
    /// while the swap's own setup can still change that: its nonce isn't settled, or a
    /// `token` note created in a transaction recorded for one of its setups is unspent with a
    /// POI state that [`crate::UtxoPpoiState::retry_eligible`] accepts. A setup without a
    /// recorded transaction waits only for settlement. Otherwise the attempt runs and reports
    /// any shortfall. Planning stays authoritative for every other limit.
    pub fn swap_placement_readiness(
        &self,
        session: &WalletSession,
        operation: ExecutorOperationId,
        token: Address,
        spend: U256,
    ) -> Result<SwapPlacementReadiness> {
        self.require_swap_session(session)?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let notes = self.swap_note_snapshot(session)?;
        let spendable = self.inputs_for_swap_order(notes.spendable, &record)?;
        Ok(placement_readiness(
            &record,
            crate::max_unshield_spendable(&spendable, token) >= spend,
            &notes.unspent,
            token,
        ))
    }

    /// Largest batched sell amount of each of `tokens` currently available to this swap, in
    /// their order. The same POI, pending-spend and executor reservation filters apply as
    /// during planning. The notes and reservations are read once for all tokens.
    pub fn max_swap_amounts(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
        tokens: &[Address],
    ) -> Result<Vec<U256>> {
        self.require_swap_session(session)?;
        let inputs = self.spendable_swap_inputs(session, operation)?;
        Ok(tokens
            .iter()
            .map(|token| crate::max_unshield_spendable(&inputs, *token))
            .collect())
    }

    fn spendable_swap_inputs(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
    ) -> Result<Vec<Utxo>> {
        match operation {
            Some(operation) => {
                let record = self
                    .swap_account_record(operation)?
                    .ok_or_else(|| eyre!("swap executor is unavailable"))?;
                self.swap_inputs(session, &record)
            }
            None => self.filter_reserved_inputs(self.spendable_swap_notes(session)?, None),
        }
    }

    pub(super) fn spendable_swap_notes(&self, session: &WalletSession) -> Result<Vec<Utxo>> {
        Ok(self.swap_note_snapshot(session)?.spendable)
    }

    /// The wallet's notes from one current snapshot: the POI-spendable ones planning starts
    /// from, and every unspent one with its POI state.
    #[cfg_attr(not(feature = "test-support"), allow(clippy::unused_self))]
    fn swap_note_snapshot(&self, session: &WalletSession) -> Result<SwapNoteSnapshot> {
        #[cfg(feature = "test-support")]
        {
            let notes = self
                .swap_notes_for_tests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !notes.is_empty() {
                return Ok(SwapNoteSnapshot {
                    spendable: notes
                        .iter()
                        .filter(|(_, state)| *state == crate::UtxoPpoiState::Valid)
                        .map(|(note, _)| note.clone())
                        .collect(),
                    unspent: notes.clone(),
                });
            }
        }
        let snapshot = session
            .handle
            .current_snapshot()
            .ok_or_else(|| eyre!("private wallet snapshot is unavailable"))?;
        let lists = crate::default_active_poi_list_keys();
        Ok(SwapNoteSnapshot {
            spendable: crate::poi_verified_unspent_utxos_from_records(
                &snapshot.utxos,
                &snapshot.pending_overlay,
            ),
            unspent: snapshot
                .utxos
                .iter()
                .filter(|entry| !entry.is_spent())
                .map(|entry| {
                    (
                        entry.utxo.clone(),
                        crate::utxos::utxo_ppoi_state(&entry.utxo.poi, &lists),
                    )
                })
                .collect(),
        })
    }

    /// Plan swaps from one spendable note holding `amount` of `token` instead of the session's
    /// notes. UI tests use it to quote through the real planning and review, since their
    /// sessions never sync.
    #[cfg(feature = "test-support")]
    pub fn plan_swaps_from_note_for_tests(&self, token: Address, amount: U256) {
        self.swap_notes_for_tests(&[(
            token,
            amount,
            B256::repeat_byte(6),
            crate::UtxoPpoiState::Valid,
        )]);
    }

    /// Hold `notes` in place of the session's, each one of `token` holding `amount`, created
    /// in the transaction `source` and in the POI state given. Only Valid notes are spendable.
    /// UI tests use it, since their sessions never sync.
    #[cfg(feature = "test-support")]
    pub fn swap_notes_for_tests(&self, notes: &[(Address, U256, B256, crate::UtxoPpoiState)]) {
        *self
            .swap_notes_for_tests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = test_swap_notes(notes);
    }

    /// The identity of each of `notes` as [`Self::swap_notes_for_tests`] holds them, which a
    /// payload that spends one records.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn swap_note_identities_for_tests(
        notes: &[(Address, U256, B256, crate::UtxoPpoiState)],
    ) -> Vec<ExecutorInputIdentity> {
        test_swap_notes(notes)
            .iter()
            .map(|(note, _)| ExecutorInputIdentity::from_utxo(note))
            .collect()
    }

    pub(super) fn require_swap_session(&self, session: &WalletSession) -> Result<()> {
        if !session
            .executor_owner()
            .is_some_and(|owner| std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!("swap wallet belongs to another session"));
        }
        Ok(())
    }

    pub(super) fn swap_order_profile(&self) -> Result<SwapProfile> {
        self.swap_executor_profile()?;
        self.chain
            .swap_profile()
            .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))
    }
}
