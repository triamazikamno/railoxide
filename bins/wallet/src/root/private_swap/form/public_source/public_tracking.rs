use super::*;

impl PrivateSwapsView {
    pub(in crate::root::private_swap::form) fn public_destination_setup_progress(
        &self,
        form: &SwapForm,
    ) -> Option<SwapSetupProgress> {
        self.public_setup_progress(form.public.as_ref()?.operation?)
    }

    /// How far the setup of the destination stealth account `operation` is, by its record.
    pub(super) fn public_setup_progress(
        &self,
        operation: ExecutorOperationId,
    ) -> Option<SwapSetupProgress> {
        let (chain, record) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)?;
        Some(
            match crate::root::private_swap::account_stage(record, *chain, None, false, None) {
                SwapStage::Ready => SwapSetupProgress::Done,
                SwapStage::SetupPending => SwapSetupProgress::Pending,
                SwapStage::SetupFailed => SwapSetupProgress::Failed,
                _ => SwapSetupProgress::NotSent,
            },
        )
    }

    pub(in crate::root::private_swap) fn refresh_public_swap_records(&mut self, cx: &App) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let root = root.read(cx);
        self.public_synced.extend(
            root.chain_states
                .iter()
                .filter(|(_, state)| matches!(state, ChainUtxoState::Ready { .. }))
                .map(|(chain, _)| *chain),
        );
        // Debug UI fixture: its synthesized records aren't read over.
        if ui_fixture::holds_records() {
            return;
        }
        self.public_railgun = root
            .effective_chain_configs
            .iter()
            .filter_map(|(&id, chain)| Some((id, chain.railgun.as_ref()?.deployment.contract)))
            .collect();
        let mut records = Vec::new();
        for (chain, state) in &root.chain_states {
            let (ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. }) =
                state
            else {
                continue;
            };
            let Some(owner) = session.executor_owner() else {
                continue;
            };
            match owner.records() {
                Ok(accounts) => records.extend(
                    accounts
                        .into_iter()
                        .filter(|record| {
                            record
                                .swap_uses()
                                .iter()
                                .any(|swap_use| swap_use.public_swap().is_some())
                        })
                        .map(|record| {
                            self.attributions.extend(
                                owner
                                    .attribution(&record)
                                    .map(|attribution| (record.operation(), attribution)),
                            );
                            (*chain, record)
                        }),
                ),
                Err(error) => self.error = Some(format!("{error:#}")),
            }
        }
        records.sort_by_key(|(_, record)| std::cmp::Reverse((record.created_at(), record.index())));
        self.public_records = records;
        // An open swap that ended, or a new one, changes what blocks the form's pair.
        self.refresh_public_conflict(cx);
    }

    /// Refreshes only the Public account that paid for this swap, on its origin chain.
    pub(super) fn refresh_public_source_balance(
        &self,
        identity: crate::root::private_swap::model::SwapIdentity,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((origin_chain, source)) = self
            .public_records
            .iter()
            .filter(|(_, record)| record.operation() == identity.operation)
            .find_map(
                |(_, record)| match record.swap_use(identity.swap_use)?.role() {
                    SwapUseRole::PublicSourceDestination {
                        origin_chain,
                        source,
                        ..
                    } => Some((*origin_chain, *source)),
                    _ => None,
                },
            )
        else {
            return;
        };
        let _ = self.root.update(cx, |root, cx| {
            // The record names its payer by address.
            let accounts = root
                .public_accounts
                .iter()
                .filter(|account| account.address == source)
                .map(|account| account.public_account_uuid.clone())
                .collect::<Vec<_>>();
            root.schedule_account_public_balance_refresh(origin_chain, accounts, None, cx);
        });
    }

    pub(in crate::root::private_swap) fn track_public_swaps(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let requests = self
            .public_records
            .iter()
            .flat_map(|(chain, record)| {
                record.public_swaps_to_track().filter_map(move |(id, _)| {
                    let SwapUseRole::PublicSourceDestination { origin_chain, .. } =
                        record.swap_use(id)?.role()
                    else {
                        return None;
                    };
                    Some((*chain, record.operation(), id, *origin_chain))
                })
            })
            .collect::<Vec<_>>();
        // Each swap is checked on its own schedule.
        let requests = self.public_schedule.poll_due(
            requests,
            |&(chain, operation, id, _)| (chain, operation, id),
            std::time::Instant::now(),
        );
        for (chain, operation, id, origin) in requests {
            self.track_public_swap_request(chain, operation, id, origin, false, window, cx);
        }
    }

    fn track_public_swap_request(
        &mut self,
        chain: u64,
        operation: ExecutorOperationId,
        id: SwapUseId,
        origin_chain: u64,
        explicit: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has no swap to track.
        if ui_fixture::holds_records() {
            return;
        }
        let key = (chain, operation, id);
        if !self.public_tracking.insert(key) {
            return;
        }
        if explicit {
            self.public_destination_balances.remove(&key);
            cx.notify();
        }
        let Some((session, owner)) = self.destination_owner(chain, cx) else {
            self.public_tracking.remove(&key);
            return;
        };
        let Some(root) = self.root.upgrade() else {
            self.public_tracking.remove(&key);
            return;
        };
        let (origin, http) = {
            let root = root.read(cx);
            (
                root.effective_chain_configs.get(origin_chain).cloned(),
                root.http.clone(),
            )
        };
        let Some(origin) = origin else {
            self.public_tracking.remove(&key);
            return;
        };
        root.update(cx, |root, _| {
            root.activate_chains_background([origin_chain, chain]);
        });
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(async move {
                let clients = public_swap_clients(&origin, &http).await?;
                owner
                    .track_public_swap(
                        operation,
                        id,
                        PublicSwapTracking {
                            origin: &origin,
                            across: &clients.across,
                            orderbook: clients.orderbook.as_ref(),
                            explicit,
                            client: &clients.http,
                        },
                    )
                    .await
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                view.public_tracking.remove(&key);
                view.public_schedule.polled(
                    &std::collections::BTreeSet::from([key]),
                    std::time::Instant::now(),
                );
                if !view.session_is_current(cx)
                    || !view
                        .destination_owner(chain, cx)
                        .is_some_and(|(current, _)| Arc::ptr_eq(&current, &session))
                {
                    return;
                }
                view.refresh_public_swap_records(cx);
                match &result {
                    Ok(Ok(progress)) => {
                        if explicit {
                            view.public_destination_balances.remove(&key);
                            if let Some(balance) = progress.destination_balance {
                                view.public_destination_balances.insert(key, balance);
                            }
                        }
                        view.public_tracking_failures.remove(&(operation, id));
                        // A warning of the latest step only: it is off again once the
                        // allowance covers the sale or the order can no longer fill.
                        if progress.permit_used_up {
                            view.public_permit_used_up.insert((operation, id));
                        } else {
                            view.public_permit_used_up.remove(&(operation, id));
                        }
                    }
                    Ok(Err(_)) => {
                        let failures = view
                            .public_tracking_failures
                            .entry((operation, id))
                            .or_default();
                        *failures = failures.saturating_add(1);
                    }
                    Err(_) => {}
                }
                match result {
                    Ok(Ok(progress)) if progress.refresh_public_balances => {
                        view.refresh_public_source_balance(
                            crate::root::private_swap::model::SwapIdentity {
                                operation,
                                swap_use: id,
                            },
                            cx,
                        );
                    }
                    // A background pass is read again on the next one, so only a check the
                    // user asked for reports its failure.
                    Ok(Err(error)) if explicit => view.error = Some(format!("{error:#}")),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root::private_swap) fn perform_public_swap_action(
        &mut self,
        identity: crate::root::private_swap::model::SwapIdentity,
        action: PublicSwapAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has nothing to act on.
        if self.busy() || ui_fixture::holds_records() {
            return;
        }
        let Some((chain, record)) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == identity.operation)
        else {
            return;
        };
        let chain = *chain;
        let Some((_, swap)) = record.public_swap_use(identity.swap_use) else {
            return;
        };
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            ..
        }) = record.swap_use(identity.swap_use).map(SwapUseRecord::role)
        else {
            return;
        };
        let (origin_chain, source, destination_token) =
            (*origin_chain, *source, *destination_token);
        let saved = swap.clone();
        let Some((session, owner)) = self.destination_owner(chain, cx) else {
            return;
        };
        match action {
            PublicSwapAction::CheckStatus => self.track_public_swap_request(
                chain,
                identity.operation,
                identity.swap_use,
                origin_chain,
                true,
                window,
                cx,
            ),
            PublicSwapAction::CancelPreparation => {
                match owner.cancel_public_swap(identity.operation, identity.swap_use) {
                    Ok(_) => {
                        self.public_execution = None;
                        self.refresh_public_swap_records(cx);
                    }
                    Err(error) => self.error = Some(format!("{error:#}")),
                }
                cx.notify();
            }
            PublicSwapAction::Continue | PublicSwapAction::RetrySetup | PublicSwapAction::Retry => {
                let fresh_retry = action == PublicSwapAction::Retry;
                if fresh_retry
                    && !crate::root::private_swap::public_progress::public_swap_can_retry(
                        &saved,
                        now_unix(),
                    )
                {
                    return;
                }
                self.reopen_public_swap_form(identity, fresh_retry, window, cx);
            }

            PublicSwapAction::Withdraw | PublicSwapAction::CancelOrder => {
                let Some(root) = self.root.upgrade() else {
                    return;
                };
                let (origin, http, account) = {
                    let root = root.read(cx);
                    (
                        root.effective_chain_configs.get(origin_chain).cloned(),
                        root.http.clone(),
                        root.public_accounts
                            .iter()
                            .find(|account| account.address == source)
                            .cloned(),
                    )
                };
                let (Some(origin), Some(account)) = (origin, account) else {
                    self.error =
                        Some("The Public source account or network is unavailable.".into());
                    cx.notify();
                    return;
                };
                self.review_public_recovery(
                    identity, session, owner, origin, http, account, action, window, cx,
                );
            }
            PublicSwapAction::RecoverDestination => {
                let checked = record
                    .swap_use(identity.swap_use)
                    .and_then(|claimed| self.public_destination_balance(record, claimed));
                let root = self.root.clone();
                window.defer(cx, move |window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.open_stealth_account_recovery_on(
                            chain,
                            identity.operation,
                            wallet_ops::vault::ExecutorAsset::Erc20(destination_token),
                            checked,
                            window,
                            cx,
                        );
                    });
                });
            }
        }
    }

    /// Bring back the form of the saved swap `identity` for a new review, on its origin
    /// network. A fresh retry leaves the swap's use behind.
    pub(super) fn reopen_public_swap_form(
        &mut self,
        identity: crate::root::private_swap::model::SwapIdentity,
        fresh_retry: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((record, claimed, saved)) = self.public_swap_record(identity) else {
            return;
        };
        let (
            Some(chain),
            SwapUseRole::PublicSourceDestination {
                origin_chain,
                source,
                destination_token,
                ..
            },
        ) = (self.public_record_chain(record), claimed.role())
        else {
            return;
        };
        let (origin_chain, source, destination_token) =
            (*origin_chain, *source, *destination_token);
        let saved = saved.clone();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let account = root
            .read(cx)
            .public_accounts
            .iter()
            .find(|account| account.address == source)
            .cloned();
        let Some(account) = account else {
            self.error = Some("The paying Public account is unavailable.".into());
            cx.notify();
            return;
        };
        if origin_chain != self.origin_chain_id {
            let root = self.root.clone();
            window.defer(cx, move |window, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.select_chain(origin_chain, window, cx);
                    root.ensure_private_swaps(window, cx);
                    if let Some(view) = root.private_swaps_view() {
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.restore_public_swap_form(
                                    identity,
                                    chain,
                                    destination_token,
                                    account,
                                    &saved,
                                    fresh_retry,
                                    window,
                                    cx,
                                );
                            });
                        });
                    }
                });
            });
            return;
        }
        self.restore_public_swap_form(
            identity,
            chain,
            destination_token,
            account,
            &saved,
            fresh_retry,
            window,
            cx,
        );
    }

    /// Restore a changed delivery for explicit consent, using the actual fees from signing.
    pub(super) fn apply_public_review_change(
        &mut self,
        command: &PublicSwapAuthorization,
        change: SwapReviewChange,
        review: Option<Arc<PublicSwapReview>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.reopen_public_swap_form(command.identity(), false, window, cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(public) = form.public.as_mut() else {
            return;
        };
        public.review_change = Some(change);
        if let (
            Some(review),
            PublicSwapCommand::Swap {
                clients,
                quote_terms,
                ..
            },
        ) = (review, &command.action)
        {
            // Cancel the generic preview started while restoring the saved draft. Its reply
            // cannot overwrite the actual quote, even if it was already on its way back.
            form.quote_revision = form.quote_revision.wrapping_add(1);
            form.quote_task = None;
            form.quote = QuoteState::Idle;
            form.quote_details_open = false;
            form.bridge_quote_error = None;
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            public.review = Some(review);
            public.clients = Some(clients.clone());
            public.quote_terms = Some(quote_terms.clone());
            public.delivery_review = true;
        }
        self.sync_gas_controls(window, cx);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_public_swap_form(
        &mut self,
        identity: crate::root::private_swap::model::SwapIdentity,
        chain: u64,
        destination_token: Address,
        account: PublicAccountMetadata,
        saved: &wallet_ops::vault::PublicSwapRecord,
        fresh_retry: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_public_form(account, saved.approval().sell_token, window, cx);
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |root, _| root.activate_chain_background(chain));
        }
        let decimals = self
            .form
            .as_ref()
            .and_then(|form| self.form_sell_metadata(form, cx))
            .map(|metadata| metadata.decimals);
        if let Some(form) = self.form.as_mut() {
            form.network = Some(chain);
            form.buy = Some(destination_token);
            form.slippage_bps = saved.approval().bounds.slippage_bps;
            form.gas_share_bps = saved
                .approval()
                .bounds
                .gas_share_bps
                .unwrap_or(GAS_SHARE_BALANCED_BPS);
            form.bridge.shield_failure = saved.approval().on_shield_failure;
            form.amount_input.update(cx, |input, cx| {
                input.set_value(
                    format_unshield_amount_input(saved.approval().bounds.sell_amount, decimals),
                    window,
                    cx,
                );
            });
            if !fresh_retry && let Some(public) = form.public.as_mut() {
                public.operation = Some(identity.operation);
                public.swap_use = Some(identity.swap_use);
            }
        }
        self.refresh_public_swap_records(cx);
        self.load_public_network_routes(chain, window, cx);
        self.refresh_destination_accounts(window, cx);
        self.refresh_setup_route(cx);
        self.schedule_public_quote(window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn review_public_recovery(
        &mut self,
        identity: crate::root::private_swap::model::SwapIdentity,
        session: Arc<WalletSession>,
        owner: Arc<ExecutorOwner>,
        origin: EffectiveChainConfig,
        http: wallet_ops::HttpContext,
        source: PublicAccountMetadata,
        action: PublicSwapAction,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let runtime = self.runtime.clone();
        self.public_job = Some(cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(async move {
                let fee = quote_public_action_gas_fee(origin.chain_id, &origin, &http).await?;
                let gas_fee = PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: fee.suggested_max_fee_per_gas,
                    max_priority_fee_per_gas: fee.suggested_max_priority_fee_per_gas,
                };
                let (command, max_gas_cost, client) = if action == PublicSwapAction::Withdraw {
                    // The recovery's route, from its review through its transaction.
                    let client = http.operation_http_client().await?;
                    let review = owner
                        .review_public_swap_withdrawal(
                            identity.operation,
                            identity.swap_use,
                            &origin,
                            fee.suggested_max_fee_per_gas,
                            fee.suggested_max_priority_fee_per_gas,
                            &client,
                        )
                        .await?;
                    (
                        PublicSwapCommand::Withdraw {
                            review,
                            fee: gas_fee,
                        },
                        review.max_gas_cost,
                        client,
                    )
                } else {
                    let clients = public_swap_clients(&origin, &http).await?;
                    let orderbook = clients
                        .orderbook
                        .ok_or_else(|| eyre::eyre!("This origin has no orderbook."))?;
                    let gas_limit = 100_000_u64
                        .checked_add(origin.gas.gas_limit_buffer)
                        .ok_or_else(|| eyre::eyre!("The cancellation gas limit overflows."))?;
                    (
                        PublicSwapCommand::Cancel {
                            fee: gas_fee,
                            gas_limit,
                            orderbook,
                        },
                        U256::from(gas_limit) * U256::from(fee.suggested_max_fee_per_gas),
                        clients.http,
                    )
                };
                Ok::<_, eyre::Report>((
                    source,
                    origin,
                    session,
                    owner,
                    command,
                    max_gas_cost,
                    client,
                ))
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.public_job = None;
                match result {
                    Ok(Ok((
                        source,
                        origin,
                        destination_session,
                        owner,
                        action,
                        max_gas_cost,
                        client,
                    ))) => {
                        // The review's amounts and accounts are formatted here, with the
                        // wallet's token metadata.
                        let summary = view.public_recovery_summary(
                            &source,
                            &origin,
                            &action,
                            max_gas_cost,
                            cx,
                        );
                        let command = Arc::new(PublicSwapAuthorization {
                            source,
                            origin,
                            destination_session,
                            owner,
                            operation: identity.operation,
                            swap_use: identity.swap_use,
                            action,
                            summary: summary.clone(),
                            revision: 0,
                            client,
                        });
                        view.public_authorization = Some(command.clone());
                        let entity = cx.entity();
                        let _ = view.root.update(cx, |root, cx| {
                            root.request_spend_authorization(
                                SpendAuthorizationIntent::PublicSwap(entity, command),
                                summary,
                                window,
                                cx,
                            );
                        });
                    }
                    Ok(Err(error)) => view.error = Some(format!("{error:#}")),
                    Err(_) => view.error = Some("Couldn't review this action. Try again.".into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The review of a withdrawal of the proceeds a `CoW` proxy holds, or of an order's
    /// cancellation: what moves, the accounts by name with their addresses to copy, and the
    /// most the Public account pays in gas.
    fn public_recovery_summary(
        &self,
        source: &PublicAccountMetadata,
        origin: &EffectiveChainConfig,
        action: &PublicSwapCommand,
        max_gas_cost: U256,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let label = public_source_label(source);
        let network = network_name(origin.chain_id);
        let account = |row: &'static str| {
            SpendAuthorizationSummaryRow::new(row, source.address.to_checksum(None))
                .with_copyable_account(format!("{label} "), "account address")
        };
        let gas = SpendAuthorizationSummaryRow::new(
            format!("Maximum gas on {network}"),
            origin.native_currency.format_amount(max_gas_cost),
        );
        let summary = match action {
            PublicSwapCommand::Withdraw { review, .. } => SpendAuthorizationSummary::new(
                format!("Withdraw to {label}"),
                format!(
                    "{label} sends one transaction that moves the {} from its CoW proxy to {label}. It costs gas on {network}.",
                    self.network_token_symbol(origin.chain_id, review.token, cx)
                ),
                vec![
                    SpendAuthorizationSummaryRow::new(
                        "Amount",
                        self.chain_amount(origin.chain_id, review.token, review.amount, cx),
                    ),
                    SpendAuthorizationSummaryRow::new("From", review.proxy.to_checksum(None))
                        .with_copyable_account("Your CoW proxy ", "CoW proxy address"),
                    account("Return to"),
                    gas,
                ],
            )
            .with_confirm_label("Withdraw"),
            PublicSwapCommand::Cancel { .. } | PublicSwapCommand::Swap { .. } => {
                SpendAuthorizationSummary::new(
                    "Cancel Public order",
                    format!(
                        "{label} sends one transaction that cancels the order. It costs gas on {network}. If the order fills before the cancellation confirms, the swap still goes through and is delivered. {label} can buy the same token again once the order's validity has passed."
                    ),
                    vec![account("Pay from"), gas],
                )
                .with_confirm_label("Cancel order")
            }
        };
        summary.requiring_explicit_review()
    }
}
