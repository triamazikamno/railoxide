use super::*;

impl ExecutorStore {
    /// Bring this chain's destination stealth accounts in line with their origin swaps on
    /// other chains, in one write. Unlinked reservations may still be in preparation and are
    /// left alone. A destination whose swap setup was stopped before any outcome is stopped
    /// and retired too, when that swap reserved it fresh; an account the swap reused only has
    /// that use stopped. Otherwise the origin's bridge outcomes decide what became of the
    /// shield payload, for each destination use from the orders of that use alone. A stopped,
    /// orderless origin releases unsigned reused claims through cancellation. Returns whether
    /// any record changed.
    pub fn reconcile_swap_destinations(&self) -> Result<bool, ExecutorStoreError> {
        self.reconcile_swap_destinations_inner(false)
    }

    /// Reconcile before the chain's owner admits work. An unlinked reservation left by a
    /// crash between the two reservations can be retired here while it has issued nothing.
    pub(crate) fn reconcile_swap_destinations_on_load(&self) -> Result<bool, ExecutorStoreError> {
        self.reconcile_swap_destinations_inner(true)
    }

    fn reconcile_swap_destinations_inner(
        &self,
        retire_orphans: bool,
    ) -> Result<bool, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut updates = Vec::new();
        let mut origins = std::collections::BTreeMap::new();
        for mut record in self.records()? {
            let previous = record.clone();
            // Every destination use is matched to its own origin use, so an earlier swap's fill
            // is never read as a later swap's.
            let uses: Vec<_> = record
                .swap_uses()
                .iter()
                .filter_map(destination_link)
                .collect();
            for (id, origin_chain, origin_operation, known) in uses {
                // Only the use that reserved an account fresh, while it still claims the
                // account, can retire it or stop its setup.
                let fresh = record.active_swap_use == Some(id)
                    && record.swap_use(id).is_some_and(SwapUseRecord::is_fresh);
                let key = (origin_chain, origin_operation);
                if let std::collections::btree_map::Entry::Vacant(entry) = origins.entry(key) {
                    entry.insert(self.for_chain(origin_chain).record(origin_operation)?);
                }
                let origin = origins
                    .get_mut(&key)
                    .and_then(Option::as_mut)
                    .filter(|origin| origin.links_swap_destination(id, record.operation));
                let next = match origin {
                    None => {
                        record.retired |= retire_orphans && fresh && record.issued.is_empty();
                        None
                    }
                    Some(origin) => {
                        let setup_stopped = origin.swap_setup_stopped && known.is_none();
                        let stopped = setup_stopped
                            || origin.swap_use(id).is_some_and(SwapUseRecord::is_stopped);
                        if stopped && !origin.has_swap_use_order(id) {
                            // Both facts come from the exact linked source while the mutation
                            // lock excludes any late order or signed-data handoff.
                            origin.cancel_swap_use(id);
                            record.cancel_swap_use(id);
                        } else if setup_stopped && fresh {
                            record.swap_setup_stopped = true;
                            record.retired = true;
                        }
                        let derived = record
                            .address
                            .and_then(|address| origin.swap_destination_outcome(id, address));
                        Some((stopped, next_destination_outcome(known, derived)))
                    }
                };
                if let Some((stop, next)) = next
                    && let Some(SwapUseRecord {
                        stopped,
                        role:
                            SwapUseRole::Destination {
                                shields, outcome, ..
                            },
                        ..
                    }) = record.swap_use_mut(id)
                {
                    *stopped |= stop;
                    // A fill can only have run, or left funds for, a shield this use signed.
                    let filled = matches!(
                        next,
                        Some(
                            SwapDestinationOutcome::Shielded { .. }
                                | SwapDestinationOutcome::Held { .. }
                        )
                    );
                    if !(filled && shields.is_empty()) {
                        *outcome = next;
                    }
                }
            }
            if record != previous {
                updates.push(self.seal(
                    RecordKind::ExecutorOperation,
                    self.operation_key(record.operation),
                    &record,
                )?);
            }
        }
        for ((chain_id, operation), origin) in origins {
            let Some(origin) = origin else {
                continue;
            };
            let store = self.for_chain(chain_id);
            if store.record(operation)?.as_ref() != Some(&origin) {
                updates.push(store.seal(
                    RecordKind::ExecutorOperation,
                    store.operation_key(operation),
                    &origin,
                )?);
            }
        }
        if updates.is_empty() {
            return Ok(false);
        }
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(true)
    }

    /// The swap record on its own chain that names this chain's destination stealth account
    /// `operation` as the destination of the use that claims it. `None` when no destination use
    /// claims `operation` or no swap references it.
    pub(crate) fn swap_destination_origin(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<Option<ExecutorRecord>, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let Some((id, origin_chain, origin_operation, _)) = self
            .record(operation)?
            .and_then(|record| record.active_use().and_then(destination_link))
        else {
            return Ok(None);
        };
        Ok(self
            .for_chain(origin_chain)
            .record(origin_operation)?
            .filter(|origin| origin.links_swap_destination(id, operation)))
    }
}
