use super::*;

impl PrivateSwapsView {
    /// The wallet session and executor owner of `chain_id`, a private Bridge swap's destination
    /// network, once that network's session is loaded.
    pub(in crate::root) fn destination_owner(
        &self,
        chain_id: u64,
        cx: &gpui::App,
    ) -> Option<(Arc<WalletSession>, Arc<ExecutorOwner>)> {
        use crate::root::chain_load::ChainUtxoState;
        let root = self.root.upgrade()?;
        match root.read(cx).chain_states.get(&chain_id)? {
            ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. } => {
                Some((Arc::clone(session), session.executor_owner()?))
            }
            _ => None,
        }
    }

    /// [`Self::destination_owner`] once `chain_id`'s private sync is ready, for a setup or an
    /// order that spends or signs there. Otherwise what the user is told. A network that isn't
    /// loaded starts loading.
    pub(super) fn ready_destination(
        &self,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) -> Result<(Arc<WalletSession>, Arc<ExecutorOwner>), String> {
        use crate::root::chain_load::ChainUtxoState;
        let network = form::network_name(chain_id);
        let root = self
            .root
            .upgrade()
            .ok_or_else(|| "The wallet session ended.".to_owned())?;
        match root.read(cx).chain_states.get(&chain_id) {
            Some(ChainUtxoState::Ready { session, .. }) => {
                let owner = session.executor_owner().ok_or_else(|| {
                    format!("Stealth accounts aren't available on {network} for this wallet.")
                })?;
                return Ok((Arc::clone(session), owner));
            }
            Some(ChainUtxoState::Syncing { .. } | ChainUtxoState::Loading { .. }) => {
                return Err(format!(
                    "{network} is still syncing. Try again once it is ready. Nothing was sent."
                ));
            }
            Some(ChainUtxoState::Error { .. }) => {
                return Err(format!(
                    "{network} couldn't be synced. Switch to {network} to see why, then try again. Nothing was sent."
                ));
            }
            None | Some(ChainUtxoState::Idle) => {}
        }
        self.ensure_destination_load(chain_id, cx);
        Err(format!(
            "{network} isn't loaded yet. It is loading now. Try again once it is ready. Nothing was sent."
        ))
    }

    /// Read each private Bridge swap's destination stealth account again from the destination
    /// network's executor records. Networks that aren't loaded are left out.
    pub(super) fn reload_destinations(&mut self, cx: &gpui::App) {
        // Debug UI fixture: its synthesized destination account isn't read over.
        if ui_fixture::holds_swaps() {
            return;
        }
        let mut loaded = BTreeMap::new();
        let mut destinations = BTreeMap::new();
        let mut attributions = Vec::new();
        for record in &self.records {
            // Each swap of a reused account names its own destination account.
            for swap_use in record.swap_uses() {
                let Some((delivery, operation)) =
                    model::swap_use_destination(record, swap_use.id())
                else {
                    continue;
                };
                let records =
                    loaded
                        .entry(delivery.destination_chain)
                        .or_insert_with_key(|chain_id| {
                            let (_, owner) = self.destination_owner(*chain_id, cx)?;
                            let records = owner.records().ok()?;
                            Some((owner, records))
                        });
                if let Some((owner, records)) = records {
                    let destination = records
                        .iter()
                        .find(|destination| destination.operation() == operation);
                    attributions.extend(destination.and_then(|destination| {
                        Some((destination.operation(), owner.attribution(destination)?))
                    }));
                    destinations.insert(
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: swap_use.id(),
                        },
                        destination.cloned(),
                    );
                }
            }
        }
        self.destinations = destinations;
        self.attributions.extend(attributions);
    }

    /// Start loading the destination network of each private Bridge swap that is still being
    /// set up, whose destination account only that network's session can tell about, then read
    /// the destination accounts again.
    pub(super) fn refresh_destinations(&mut self, cx: &mut Context<'_, Self>) {
        // Debug UI fixture: its synthesized swap loads no other network.
        if ui_fixture::holds_swaps() {
            return;
        }
        let chains = self
            .records
            .iter()
            .filter(|record| {
                let cancelled = model::holds_only_cancelled_preparations(record)
                    && self.pending_order(record).is_none();
                record.swap().is_none()
                    && !record.is_retired()
                    && !record.is_swap_setup_stopped()
                    && !cancelled
            })
            .filter_map(swap_private_delivery)
            .map(|delivery| delivery.destination_chain)
            // A reused account's prepared swap waits for a new destination account's setup
            // the same way.
            .chain(self.records.iter().filter_map(|record| {
                let swap_use = model::prepared_swap_use(record)?;
                sets_up_destination(swap_use).then_some(())?;
                Some(
                    swap_use
                        .approval()?
                        .delivery
                        .private_bridge()?
                        .destination_chain,
                )
            }))
            .collect::<BTreeSet<_>>();
        for chain_id in chains {
            self.ensure_destination_load(chain_id, cx);
        }
        let known = std::mem::take(&mut self.destinations);
        self.reload_destinations(cx);
        if self.destinations != known {
            cx.notify();
        }
    }

    /// Start loading `chain_id`'s wallet session unless it is loaded, loading, or failed to
    /// load. The root starts it after this update, since a load replaces per-chain views.
    pub(super) fn ensure_destination_load(&self, chain_id: u64, cx: &mut Context<'_, Self>) {
        use crate::root::chain_load::ChainUtxoState;
        let idle = self.root.upgrade().is_some_and(|root| {
            matches!(
                root.read(cx).chain_states.get(&chain_id),
                None | Some(ChainUtxoState::Idle)
            )
        });
        if !idle {
            return;
        }
        let root = self.root.clone();
        cx.defer(move |cx| {
            let _ = root.update(cx, |root, cx| root.ensure_chain_load(chain_id, cx));
        });
    }

    /// The swap `record`'s own progress shows: the draft's while a new order is being
    /// prepared, otherwise the latest order's, or before any order the one that claims the
    /// account.
    pub(super) fn shown_swap(&self, record: &ExecutorRecord) -> Option<SwapIdentity> {
        let swap_use = self.pending_order(record).map_or_else(
            || model::record_latest_use(record),
            |pending| Some(pending.swap_use),
        )?;
        Some(SwapIdentity {
            operation: record.operation(),
            swap_use,
        })
    }

    /// The swap `order` of `record` belongs to; without an order, the swap the record shows.
    pub(super) fn order_swap(
        &self,
        record: &ExecutorRecord,
        order: Option<&SwapOrderRecord>,
    ) -> Option<SwapIdentity> {
        order
            .and_then(SwapOrderRecord::use_id)
            .map(|swap_use| SwapIdentity {
                operation: record.operation(),
                swap_use,
            })
            .or_else(|| self.shown_swap(record))
    }

    /// The destination stealth account record of the private Bridge swap `record` shows, when
    /// its network's records were read and hold the account `delivery` names.
    pub(super) fn destination_account(
        &self,
        record: &ExecutorRecord,
        delivery: BridgeDelivery,
    ) -> Option<&ExecutorRecord> {
        self.swap_destination_account(self.shown_swap(record)?, delivery)
    }

    /// [`Self::destination_account`] for one swap of a stealth account, which an earlier swap
    /// of a reused account keeps apart from the account's later ones.
    pub(super) fn swap_destination_account(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
    ) -> Option<&ExecutorRecord> {
        self.destinations
            .get(&swap)?
            .as_ref()
            .filter(|destination| destination.address() == Some(delivery.receiver))
    }

    /// The destination stealth account record the swap `swap` claims, for its setup's retry.
    /// A preparation interrupted before the account was derived left `delivery` with its
    /// placeholder receiver. Until the receiver is bound, the account is the record the swap's
    /// source use names by its operation, which holds that use for this swap.
    pub(super) fn claimed_destination_account(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
    ) -> Option<&ExecutorRecord> {
        if !delivery.receiver.is_zero() {
            return self.swap_destination_account(swap, delivery);
        }
        self.destinations
            .get(&swap)?
            .as_ref()
            .filter(|destination| {
                destination
                    .swap_destination_use(swap.swap_use)
                    .is_some_and(|served| {
                        served.origin_chain == self.origin_chain_id
                            && served.origin_operation == swap.operation
                    })
            })
    }

    /// How far the setup of a private Bridge swap's destination stealth account is, before the
    /// swap's first order.
    pub(super) fn destination_setup_progress(
        &self,
        record: &ExecutorRecord,
        delivery: BridgeDelivery,
    ) -> SwapSetupProgress {
        match self.shown_swap(record) {
            Some(swap) => self.swap_destination_setup_progress(swap, delivery),
            None => SwapSetupProgress::NetworkLoading,
        }
    }

    /// [`Self::destination_setup_progress`] for one swap of a stealth account, which a draft
    /// on a reused account asks about before its swap is the one the record shows.
    pub(super) fn swap_destination_setup_progress(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
    ) -> SwapSetupProgress {
        let operation = swap.operation;
        match self.destinations.get(&swap) {
            None => SwapSetupProgress::NetworkLoading,
            Some(None) => SwapSetupProgress::NotSent,
            Some(Some(destination)) => account_setup_progress(
                destination,
                delivery.destination_chain,
                self.tracking
                    .get(&operation)
                    .and_then(|tracking| tracking.destination_setup),
                self.submitting_setup(operation),
            ),
        }
    }

    /// Have the destination network's owner, when that network is loaded, settle the
    /// destination stealth account of the private Bridge swap `operation` from the swap's
    /// recorded bridge outcome. It runs on the runtime, and that owner's change is picked up
    /// with the next read of the destination accounts.
    ///
    /// A fill verified as shielded consumed the account's execution nonce, which only a read
    /// of the account records. Until then its shield counts as outstanding and the account
    /// can't be chosen for another swap. So the same owner reads the account once at its
    /// network's confirmed block, through the wallet's network context. Nothing is asked
    /// while that network isn't loaded: the account then stays unavailable until the
    /// network is loaded and the swap is checked.
    pub(super) fn reconcile_destination(&self, operation: ExecutorOperationId, cx: &gpui::App) {
        let Some(delivery) = self.record(operation).and_then(swap_private_delivery) else {
            return;
        };
        let chain_id = delivery.destination_chain;
        let Some((_, owner)) = self.destination_owner(chain_id, cx) else {
            return;
        };
        let (Some(origin), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        drop(self.runtime.spawn(async move {
            let settling = Arc::clone(&owner);
            // A failed write is repeated by the owner's next reconciliation.
            let _ =
                tokio::task::spawn_blocking(move || settling.reconcile_swap_destinations()).await;
            let reading = Arc::clone(&owner);
            let delivered = tokio::task::spawn_blocking(move || {
                delivered_destinations(&origin, &reading, operation, chain_id)
            })
            .await
            .unwrap_or_default();
            if delivered.is_empty() {
                return;
            }
            // This reconciliation's own route. A failed read, or a route that can't open, leaves
            // the account unavailable until the swap is checked again.
            let Ok(client) = http.operation_http_client().await else {
                return;
            };
            for account in delivered {
                let _ = Box::pin(owner.reconcile_account(account, &client)).await;
            }
        }));
    }

    /// Whether a recovery on the destination network returned the proceeds a private Bridge
    /// swap's destination stealth account held. Only that network's loaded records tell.
    pub(super) fn held_proceeds_recovered(
        &self,
        record: &ExecutorRecord,
        delivery: BridgeDelivery,
    ) -> bool {
        let held = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .and_then(|order| match order.observations().bridge_outcome {
                Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) => Some(block.number),
                _ => None,
            });
        held.zip(self.destination_account(record, delivery))
            .is_some_and(|(block, destination)| {
                model::held_proceeds_recovered(
                    destination,
                    delivery.destination_token,
                    block,
                    self.attribution(destination),
                    self.destination_balance(record),
                )
            })
    }

    /// This session's last explicit check of a private Bridge swap's held proceeds, while it
    /// was made for the order the swap last placed, that order's proceeds are held, and it was
    /// read no earlier than their fill. The tracking outlives the order, and blocks of
    /// different destination networks don't compare, so a check of another order never counts.
    pub(super) fn destination_balance(
        &self,
        record: &ExecutorRecord,
    ) -> Option<(U256, alloy::eips::BlockNumHash)> {
        let order = record.swap()?.orders().last()?;
        let Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) =
            order.observations().bridge_outcome
        else {
            return None;
        };
        let (uid, balance, read_at) = self
            .tracking
            .get(&record.operation())?
            .destination_balance?;
        (uid == order.uid() && read_at.number >= block.number).then_some((balance, read_at))
    }
}
