use super::*;

impl PrivateSwapsView {
    /// Account-specific reconciliation is an explicit action, separate from private
    /// whole-block confirmation. An orderbook expiry report alone cannot free inputs.
    pub(super) fn check_order_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has nothing to check.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        let Some(confirmed) = self.confirmed_block(cx) else {
            self.fail(
                operation,
                "Status is unavailable until the network has synced. Try again once sync resumes."
                    .into(),
            );
            cx.notify();
            return;
        };
        let Some(mut cursor) = self
            .record(operation)
            .and_then(crate::root::private_swap::model::swap_observation_start)
        else {
            self.fail(
                operation,
                "This order's saved history is unavailable, so its status could not be checked."
                    .into(),
            );
            cx.notify();
            return;
        };
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        let route = self.swap_route(&http, operation);
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                // The check reads the account over a route of its own.
                let check = http.operation_http_client().await?;
                let fee_uid = loop {
                    let range =
                        crate::root::private_swap::model::swap_observation_range(cursor, confirmed);
                    cursor = range.end;
                    let report = owner.observe_swap(operation, range, &check).await?;
                    if cursor > confirmed
                        || !crate::root::private_swap::model::swap_stage(
                            report.record(),
                            None,
                            false,
                            owner.attribution(report.record()).as_ref(),
                        )
                        .is_observed()
                    {
                        break record_outcome_order(report.record())
                            .filter(|order| needs_executed_fee(order))
                            .map(SwapOrderRecord::uid);
                    }
                };
                // A recorded trade still without its fee asks the orderbook once more on the
                // swap's own route. A failed read leaves the CoW fee row out.
                let Some(uid) = fee_uid else {
                    return Ok(None);
                };
                let client = route.orderbook(&owner).await.ok();
                if let Some(client) = &client {
                    let _ = Box::pin(owner.observe_swap_executed_fee(operation, uid, client)).await;
                }
                Ok(Some((uid, client)))
            },
            move |this, fee, _, _| {
                let uid = this
                    .record(operation)
                    .and_then(record_outcome_order)
                    .map(SwapOrderRecord::uid);
                let tracking = this.tracking.entry(operation).or_default();
                tracking.error = None;
                tracking.status_checked = uid.map(|uid| (uid, now_unix()));
                if let Some((uid, client)) = fee {
                    tracking.fee_asked.insert(uid);
                    if client.is_some() {
                        tracking.set_route(client);
                    }
                }
            },
            window,
            cx,
        );
    }

    /// A Bridge swap's explicit check. A deposit that needs attention, or whose delivery Across
    /// only reported, is asked about again on a fresh route of the swap's own, with the
    /// destination network's settings; a refund, then or already recorded, and a deposit that
    /// wasn't sent are looked for in the stealth account.
    pub(super) fn check_bridge_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some(order) = record.swap().and_then(|swap| swap.orders().last()) else {
            return;
        };
        let (destination_chain, network) = match order.delivery() {
            SwapDelivery::Bridge(delivery) => (
                Some(delivery.destination_chain),
                network_name(delivery.destination_chain),
            ),
            // A non-EVM network has no chain to turn on or read.
            SwapDelivery::NonEvmBridge(delivery) => {
                (None, delivery.network.display_name().to_owned())
            }
            SwapDelivery::Reshield | SwapDelivery::External { .. } => return,
        };
        let stage = self.stage(record);
        let across_refund =
            stage == SwapStage::Order(SwapOrderState::Refunding) && refunds_across_deposit(record);
        let across_reported =
            stage == SwapStage::Order(SwapOrderState::Done) && reported_by_across(record);
        // The provider is asked about the attempt that holds the hand-off.
        let uid = SwapAttempts::ending_at(record, order)
            .bridge_evidence()
            .uid();
        let buy = record
            .swap()
            .map(|swap| swap.order_terms(order).buy_token());
        // `Some(None)` for an EVM chain that isn't enabled here.
        let destination = destination_chain.map(|chain_id| {
            self.root.upgrade().and_then(|root| {
                root.read(cx)
                    .effective_chain_configs
                    .get(chain_id)
                    .filter(|chain| chain.enabled)
                    .cloned()
            })
        });
        let tracking = self.tracking.get(&operation);
        let client = tracking.and_then(|tracking| tracking.orderbook.clone());
        let clients = tracking.and_then(|tracking| tracking.bridge_clients.clone());
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                // The check's own route, opened once when it first reads: the provider's
                // replacement route, and the stealth account's reads.
                let check = wallet_ops::OperationHttpClientSlot::new(http);
                let asked = if stage == SwapStage::Order(SwapOrderState::NeedsAttention)
                    || across_refund
                    || across_reported
                {
                    let destination = destination.map_or(Ok(None), |chain| {
                        chain.map(Some).ok_or_else(|| {
                            eyre::eyre!(
                                "Turn on {network} in Settings to check this swap's delivery."
                            )
                        })
                    });
                    let ask = async {
                        let destination = destination?;
                        // An explicit check leaves the route the provider may have rejected.
                        let route = check.client().await?;
                        let (client, clients) =
                            if let (Some(client), Some(clients)) = (client, clients) {
                                owner.swap_clients_on_fresh_route(&client, &clients, &route)?
                            } else {
                                let client = owner.swap_orderbook_client(&route)?;
                                let clients = owner.swap_bridge_clients(&client)?;
                                (client, clients)
                            };
                        let outcome = Box::pin(owner.check_swap_bridge(
                            operation,
                            uid,
                            &clients,
                            destination.as_ref(),
                            &route,
                        ))
                        .await?;
                        Ok::<_, eyre::Report>((client, clients, outcome))
                    };
                    Some(ask.await)
                } else {
                    None
                };
                // The refund is on this network, so a failed provider or destination check
                // still looks for it in the stealth account.
                let (route, refunding, problem) = match asked {
                    Some(Ok((client, clients, outcome))) => (
                        Some((client, clients)),
                        outcome == Some(SwapBridgeOutcome::Refunding),
                        None,
                    ),
                    Some(Err(error)) if across_refund => (None, true, Some(format!("{error:#}"))),
                    Some(Err(error)) => return Err(error),
                    None => (
                        None,
                        stage == SwapStage::Order(SwapOrderState::Refunding),
                        None,
                    ),
                };
                let balance = match buy {
                    Some(buy)
                        if refunding || stage == SwapStage::Order(SwapOrderState::NotDelivered) =>
                    {
                        let asset = ExecutorAsset::Erc20(buy);
                        let inspection = owner
                            .inspect_record(operation, &[asset], &check.client().await?)
                            .await?;
                        inspection
                            .balances()
                            .get(&asset)
                            .copied()
                            .flatten()
                            .map(|balance| (balance, inspection.block()))
                    }
                    _ => None,
                };
                Ok((route, balance, problem))
            },
            move |this, (route, balance, problem), _, _| {
                let tracking = this.tracking.entry(operation).or_default();
                if let Some((client, clients)) = route {
                    tracking.set_route(Some(client));
                    tracking.bridge_clients = Some(clients);
                }
                if balance.is_some() {
                    tracking.stealth_balance = balance;
                }
                tracking.error = None;
                if let Some(problem) = problem {
                    this.fail(operation, problem);
                }
            },
            window,
            cx,
        );
    }

    pub(super) fn resubmit_order(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        let route = self.swap_route(&http, operation);
        self.start_job(
            operation,
            SwapJobKind::Order,
            async move {
                let client = route.orderbook(&owner).await?;
                let outcome = owner.resubmit_swap_order(operation, &client).await?;
                Ok((client, outcome))
            },
            move |this, (client, outcome), window, cx| {
                this.tracking
                    .entry(operation)
                    .or_default()
                    .set_route(Some(client));
                this.finish_order(operation, outcome, window, cx);
            },
            window,
            cx,
        );
    }

    /// Recovery lives in Stealth accounts; open it there for this swap's account, on top of the
    /// swap dialog, which gets focus back when recovery closes or doesn't open.
    pub(super) fn recover(
        &self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: no wallet holds a synthesized record's account.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some((token, (_, buy))) = self.recovery_token(record).zip(swap_tokens(record)) else {
            return;
        };
        // Recovery offers what a balance check found, so this swap's own check goes with it.
        let checked = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.stealth_balance)
            .filter(|_| token == buy);
        let Some(session) = self.private_session() else {
            return;
        };
        let target = StealthAccountTarget::new(session, operation);
        let return_focus = self.swap_dialog_focus();
        let _ = self.root.update(cx, |root, cx| {
            root.open_stealth_account_recovery(
                &target,
                ExecutorAsset::Erc20(token),
                checked,
                return_focus,
                window,
                cx,
            );
        });
    }

    /// The token recovery of `record`'s stealth account starts with. It follows the account's
    /// latest order, which holds the funds, whatever a draft for another swap on it proposes.
    pub(in crate::root::private_swap) fn recovery_token(
        &self,
        record: &ExecutorRecord,
    ) -> Option<Address> {
        let (sell, buy) = swap_tokens(record)?;
        Some(swap_recovery_token(
            self.stage(record),
            &swap_delivery(record),
            sell,
            buy,
        ))
    }

    /// Held proceeds are recovered in Stealth accounts on the destination network. Switching
    /// to that network closes this dialog and replaces this view, so the root does both after
    /// this update: it switches, then opens the destination stealth account's recovery with
    /// this session's last check of its balance.
    pub(super) fn recover_on_destination(
        &self,
        operation: ExecutorOperationId,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self.record(operation) else {
            return;
        };
        let Some(delivery) = swap_private_delivery(record) else {
            return;
        };
        let Some(destination) = self.destination_account(record, delivery) else {
            return;
        };
        let destination = destination.operation();
        let checked = self.destination_balance(record);
        let root = self.root.clone();
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |root, cx| {
                root.open_stealth_account_recovery_on(
                    delivery.destination_chain,
                    destination,
                    ExecutorAsset::Erc20(delivery.destination_token),
                    checked,
                    window,
                    cx,
                );
            });
        });
    }

    /// An explicit check of held proceeds, through the destination network's owner: it settles
    /// the destination stealth account's record from this swap's outcome, then reads the
    /// delivered token's balance there, which recovery opens with.
    pub(super) fn check_destination_status(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(delivery) = self.record(operation).and_then(swap_private_delivery) else {
            return;
        };
        let chain_id = delivery.destination_chain;
        let destination = self
            .record(operation)
            .and_then(|record| self.destination_account(record, delivery))
            .map(ExecutorRecord::operation);
        let Some(((_, owner), destination)) = self.destination_owner(chain_id, cx).zip(destination)
        else {
            self.ensure_destination_load(chain_id, cx);
            self.reload_destinations(cx);
            self.fail(
                operation,
                format!(
                    "{} isn't loaded yet. Check again once it has synced.",
                    network_name(chain_id)
                ),
            );
            cx.notify();
            return;
        };
        // The order checked, so a later order's held proceeds never take this balance.
        let order = self
            .record(operation)
            .and_then(|record| record.swap()?.orders().last())
            .map(SwapOrderRecord::uid);
        let asset = ExecutorAsset::Erc20(delivery.destination_token);
        let Some(http) = self.http(cx) else {
            return;
        };
        self.start_job(
            operation,
            SwapJobKind::Check,
            async move {
                owner.reconcile_swap_destinations()?;
                let client = http.operation_http_client().await?;
                owner.read_record_balance(destination, asset, &client).await
            },
            move |this, (balance, read_at), _, _| {
                let tracking = this.tracking.entry(operation).or_default();
                if let Some(order) = order {
                    tracking.destination_balance = Some((order, balance, read_at));
                }
                tracking.error = None;
            },
            window,
            cx,
        );
    }

    /// Removal hides the swap's account, which is persisted with the encrypted record. The swap
    /// stays in My orders, and an account that needs attention stays listed in Stealth accounts.
    pub(super) fn remove_from_private_tab(
        &mut self,
        operation: ExecutorOperationId,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record isn't stored, so it can't be hidden.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        let Some(origin) = self.private_owner() else {
            return;
        };
        if let Err(error) = origin.set_hidden(operation, true) {
            self.fail(operation, error.to_string());
            cx.notify();
            return;
        }
        if let Some(tracking) = self.tracking.get_mut(&operation) {
            tracking.pending_order = None;
        }
        self.reload_records();
        cx.notify();
    }
}
