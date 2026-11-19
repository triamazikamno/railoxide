use super::*;

impl ExecutorOwner {
    /// Sign the pre-hook at the executor's current nonce `k`, for Reshield and Across delivery
    /// the post-hook at `k + 1`, and the order, after checking cached anchors. The first order
    /// after setup must match the pair and delivery approved with the setup, an External
    /// receiver must pass [`SwapProfile::check_receiver`], a public Bridge receiver the
    /// destination chain's [`crate::settings::BridgeDestinationProfile::check_receiver`], and a
    /// non-EVM receiver its network's parser in canonical form. A Bridge order's provider is
    /// quoted again before anything is signed: Across for the deposit's terms, NEAR Intents for a
    /// verified deposit address that the order pays, which 1Click may refuse for a non-EVM
    /// receiver. An Across quote below the approved destination minimum is taken once more for a
    /// deposit raised within the approval's cushion, which the order then signs. Persist
    /// them with the input reservation and the provider's terms, then submit. Nothing signed
    /// leaves the wallet before the write succeeds.
    ///
    /// On the swap's own included setup `k` is the nonce after the setup's, which the record
    /// leaves unresolved. The account is read at the latest block again under the lock the
    /// write holds, and without the setup included there nothing is written or sent: the error
    /// is [`SwapAccountRefusal::SetupUnconfirmed`].
    ///
    /// A private Bridge delivery's receiver is the wallet's own destination stealth account.
    /// Before the provider's quote and before this chain's account signs anything, that account
    /// signs its guarded shield for the approved destination minimum, which is persisted in its
    /// record on the destination chain. The Across quote then carries the destination chain's
    /// handler and the message that runs that shield, and the post-hook's deposit signs both.
    pub(crate) async fn issue_swap_order(
        &self,
        signing: SwapOrderSigning<'_>,
    ) -> Result<SwapOrderOutcome> {
        let SwapOrderSigning {
            review,
            swap_use,
            private_minimum,
            price_acknowledged,
            transactions,
            inputs,
            change_output_pois,
            output_pois,
            authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
            destination,
            continuing_minimum,
        } = signing;
        let client = orderbook.http();
        let plan = &review.plan;
        let delegated = require_delegated_plan(plan)?;
        let (operation, executor) = (delegated.operation(), delegated.executor());
        let buy_amount =
            review.require_approval(private_minimum, destination_minimum, price_acknowledged)?;
        // The attempt records the gas the signed minimum leaves room for.
        let gas_allowance = review.gas_allowance_for(private_minimum)?;
        let bridge = bridge_route(plan, bridge)?;
        if plan.delivery.private_bridge().is_some() != destination.is_some() {
            return Err(eyre!(DESTINATION_ACCOUNT_MISMATCH));
        }
        let profile = self.swap_order_profile()?;
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        require_swap_use(&record, swap_use)?;
        let account_use = order_account_use(plan, swap_use)?;
        require_swap_account(
            &record,
            self.chain.chain_id,
            SwapAccountRole::Source,
            account_use,
            delegated.evidence(),
        )?;
        require_swap_use_terms(&record, swap_use, plan)?;
        // The hooks are issued against the latest reconciled observation, which the write
        // below requires to be unchanged; the plan's proof shape fixes the nonce. That is the
        // observed nonce, or on the swap's own included setup, which the record leaves
        // unresolved at the observed nonce, the next one, which the read at the latest block
        // showed. The hooks still name the confirmed observation.
        let observed = record
            .nonce_observation()
            .ok_or_else(|| eyre!("the swap executor's nonce changed; plan the swap again"))?;
        let nonce = delegated
            .inclusion()
            .map_or_else(
                || Some(observed.nonce()),
                |inclusion| {
                    observed
                        .nonce()
                        .checked_add(U256::ONE)
                        .filter(|next| *next == inclusion.nonce())
                },
            )
            .filter(|nonce| *nonce == plan.context.execution_nonce)
            .ok_or_else(|| eyre!("the swap executor's nonce changed; plan the swap again"))?;
        if record.address() != Some(executor) || record.delegate() != delegated.delegate() {
            return Err(eyre!("the swap executor changed; plan the swap again"));
        }
        // A speed-up is signed only against the open order it was reviewed for, at that order's
        // nonce and fixed terms, and only for a lower buy amount.
        let speed_up_open = match plan.speed_up {
            Some(speed_up) => {
                let (open, _) = swap_speed_up_binding(&record, speed_up.order)
                    .ok_or_else(|| eyre!(SPEED_UP_ORDER_CHANGED))?;
                require_speed_up_window(
                    &record,
                    &profile,
                    &speed_up,
                    SystemTime::now(),
                    delegated.inclusion(),
                )?;
                if nonce != speed_up.pre_hook_nonce {
                    return Err(eyre!(SPEED_UP_NONCE_CHANGED));
                }
                let bounds = open.bounds();
                if plan.invalidates != Some(speed_up.order)
                    || review.sell_amount != bounds.sell_amount
                    || plan.amount() != bounds.spend_amount()
                    || plan.delivery != open.delivery()
                    || review.slippage_bps != bounds.slippage_bps
                {
                    return Err(eyre!(SPEED_UP_TERMS_CHANGED));
                }
                let current = review.valid_for_secs()?;
                if let Some(approved) = bounds.valid_for_secs
                    && approved != current
                {
                    return Ok(SwapOrderOutcome::ReviewRequired(
                        SwapReviewChange::Validity {
                            approved: u64::from(approved),
                            current: u64::from(current),
                        },
                    ));
                }
                if let Some(change) = speed_up_buy_amount_change(bounds, buy_amount) {
                    return Ok(SwapOrderOutcome::ReviewRequired(change));
                }
                Some(bounds)
            }
            None => None,
        };
        // A private delivery's receiver is the stealth account this swap's record links to on
        // the destination chain, confirmed there for the same wallet.
        if let (Some(delivery), Some(destination)) = (plan.delivery.private_bridge(), destination)
            && (destination.owner.chain.chain_id != delivery.destination_chain
                || !self.view.is_same_wallet_session(&destination.owner.view)
                || destination.delegated.executor() != delivery.receiver
                || record.destination_operation() != Some(destination.delegated.operation()))
        {
            return Err(eyre!(
                "the stealth account on the destination network doesn't belong to this swap; plan the swap again"
            ));
        }
        // The first order of a use is signed for the accounts its approval binds: this account,
        // a private delivery's receiver, and whether the use set each of them up.
        let first_order = !record.has_swap_use_order(swap_use);
        if first_order
            && let Some(approval) = record.swap_use(swap_use).and_then(SwapUseRecord::approval)
            && let Some(accounts) = approval.accounts
        {
            let source_setup = record
                .swap_use(swap_use)
                .is_none_or(SwapUseRecord::is_fresh);
            let bound_destination = match (plan.delivery.private_bridge(), destination) {
                (Some(delivery), Some(destination)) => {
                    let setup = destination
                        .owner
                        .swap_account_record(destination.delegated.operation())?
                        .and_then(|account| account.swap_use(swap_use).map(SwapUseRecord::is_fresh))
                        .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
                    Some((delivery.receiver, setup))
                }
                _ => None,
            };
            if !accounts.admits((executor, source_setup), bound_destination) {
                return Ok(SwapOrderOutcome::ReviewRequired(SwapReviewChange::Accounts));
            }
            // An account with earlier orders skips the check below, so its approved delivery
            // is compared here.
            if record.swap().is_some() && approval.delivery != plan.delivery {
                return Ok(SwapOrderOutcome::ReviewRequired(SwapReviewChange::Delivery));
            }
        }
        // The first order after setup places the approved pair and delivery. Later orders, with
        // their own reviews, aren't bound by it. A new delivery needs a new approval.
        if record.swap().is_none()
            && let Some(approval) = record.swap_approval()
        {
            if record.swap_approval_tokens() != Some((plan.sell_token, plan.buy_token)) {
                return Err(eyre!(
                    "this order's tokens differ from the ones approved with the swap's setup"
                ));
            }
            if approval.delivery != plan.delivery {
                return Ok(SwapOrderOutcome::ReviewRequired(SwapReviewChange::Delivery));
            }
        }
        if let SwapDelivery::External { receiver } = plan.delivery {
            profile.check_receiver(
                self.chain.require_railgun()?.deployment.contract,
                executor,
                receiver,
            )?;
        }
        // A private delivery's receiver is the wallet's own account, checked above.
        match &bridge {
            Some((
                _,
                BridgeLeg::Chain {
                    delivery, chain, ..
                },
            )) if !delivery.is_private() => {
                chain
                    .bridge_destination()
                    .ok_or_else(|| eyre!("the destination network doesn't support bridging"))?
                    .check_receiver(
                        chain
                            .railgun
                            .as_ref()
                            .map(|railgun| railgun.deployment.contract),
                        delivery.receiver,
                    )?;
            }
            // The receiver must be in the canonical form the network's parser gives it.
            Some((_, BridgeLeg::NonEvm { delivery, .. }))
                if delivery.network.parse_receiver(&delivery.receiver)? != delivery.receiver =>
            {
                return Err(eyre!("the receiver isn't in its network's canonical form"));
            }
            _ => {}
        }
        // The pre-hook must invalidate every earlier order that could fill with its funds.
        if swap_invalidation(&record, &profile, SystemTime::now())?
            .is_some_and(|live| plan.invalidates != Some(live))
        {
            return Err(eyre!(
                "an earlier order of this swap can still fill; plan the swap again"
            ));
        }
        if transactions.len() != plan.transaction_count() {
            return Err(eyre!("the proved pre-hook differs from the planned notes"));
        }
        let nullifying_key = self.view.scan_keys().nullifying_key;
        require_input_nullifiers(
            &transactions,
            inputs
                .iter()
                .map(|input| (input.tree, B256::from(input.nullifier(nullifying_key))))
                .collect(),
        )?;
        self.ensure_active()?;
        let anchors =
            match self.recheck_swap_review(review, &profile, anchor_cache, token_registry)? {
                SwapRecheck::Current(anchors) => anchors,
                SwapRecheck::Changed(change) => {
                    return Ok(SwapOrderOutcome::ReviewRequired(change));
                }
            };
        // A private Bridge delivery shields on the destination chain at that chain's fee, the
        // shared protocol constant like this chain's. The reviewed rate must be the current
        // one, and for the first order after setup the one approved with it.
        let private_quote = match plan.delivery.private_bridge() {
            Some(_) => {
                let quote = review
                    .bridge
                    .and_then(|bridge| bridge.private)
                    .ok_or_else(|| eyre!("quote the bridge before approving this swap"))?;
                let current = RAILGUN_PROTOCOL_FEE_BPS;
                let reviewed = quote.destination_shield_fee_bps;
                let approved = record
                    .swap_approval()
                    .filter(|_| record.swap().is_none())
                    .map_or(reviewed, |approval| {
                        approval
                            .bounds
                            .destination_shield_fee_bps
                            .unwrap_or_default()
                    });
                if reviewed != current || approved != current {
                    return Ok(SwapOrderOutcome::ReviewRequired(
                        SwapReviewChange::DestinationShieldFee {
                            approved: if approved == current {
                                reviewed
                            } else {
                                approved
                            },
                            current,
                        },
                    ));
                }
                Some(quote)
            }
            None => None,
        };

        // Bridge reviews carry the profile's window.
        let valid_for_secs = review.valid_for_secs()?;
        let valid_to = valid_to_after(SystemTime::now(), review.valid_for)?;
        // The provider quotes the approved order before anything is signed. A 1Click quote
        // names the receiver, so it follows every check above.
        let (bridge_terms, private_delivery, raised) =
            match &bridge {
                Some((clients, leg)) => {
                    let destination_minimum = destination_minimum.ok_or_else(|| {
                        eyre!("a bridge swap needs its approved destination minimum")
                    })?;
                    // The destination account signs its shield for the approved destination
                    // minimum, the deposit's output amount. Its payload is durable on the
                    // destination chain before the quote request carries it out of the wallet.
                    let private_leg = leg.private_delivery();
                    let private_delivery = match (destination, private_leg) {
                        (Some(destination), Some((delivery, chain))) => {
                            let handler = chain
                                .bridge_profile()
                                .ok_or_else(|| {
                                    eyre!("the destination network doesn't support bridging")
                                })?
                                .multicall_handler();
                            let destination_executor = destination.delegated.executor();
                            let shield_multicall = trace_step(
                                "order_destination_shield",
                                self.while_active(destination.owner.issue_swap_destination_shield(
                                    destination.delegated,
                                    swap_use,
                                    delivery.destination_token,
                                    destination_minimum,
                                    destination.authorization,
                                    destination.notes,
                                    client,
                                )),
                            )
                            .await?;
                            // Without a fallback a failing shield reverts the fill, and Across
                            // refunds the deposit on this chain.
                            let keep = delivery.private.is_some_and(|private| {
                                private.on_shield_failure == BridgeShieldFailure::KeepOnDestination
                            });
                            Some(AcrossPrivateDelivery {
                                handler,
                                destination_executor,
                                shield_multicall,
                                fallback: keep.then_some(destination_executor),
                            })
                        }
                        (None, None) => None,
                        _ => return Err(eyre!(DESTINATION_ACCOUNT_MISMATCH)),
                    };
                    let message = private_delivery.as_ref().zip(private_leg).map(
                        |(private, (delivery, _))| {
                            private_delivery_message(
                                private.handler,
                                delivery.destination_token,
                                private.destination_executor,
                                private.shield_multicall.clone(),
                                private.fallback,
                            )
                        },
                    );
                    let handler_message = private_delivery.as_ref().zip(message.as_ref()).map(
                        |(private, message)| AcrossHandlerMessage {
                            handler: private.handler,
                            message,
                        },
                    );
                    let mut signing = trace_step(
                        "order_bridge_quote",
                        self.while_active(self.bridge_signing_terms(
                            review,
                            clients,
                            leg,
                            buy_amount,
                            destination_minimum,
                            valid_to,
                            handler_message,
                            &profile,
                            anchor_cache,
                            token_registry,
                        )),
                    )
                    .await?;
                    // An Across quote that fell short within the approval's cushion is taken again
                    // for the deposit that delivers the approved minimum. The destination shield
                    // stays the one signed for that minimum. A NEAR Intents quote creates a deposit
                    // address, so it is never requested twice.
                    let mut raised = None;
                    if let BridgeSigning::Changed(SwapReviewChange::DestinationMinimum {
                        current,
                        ..
                    }) = signing
                        && matches!(
                            leg,
                            BridgeLeg::Chain { delivery, .. }
                                if delivery.provider == BridgeProvider::Across
                        )
                    {
                        // The allowed gas of the approval that binds a first order, else the
                        // review's own.
                        let approved_allowance = record
                            .swap_approval()
                            .filter(|_| record.swap().is_none())
                            .and_then(|approval| approval.bounds.gas_allowance)
                            .unwrap_or_else(|| review.gas_allowance());
                        let deposit = review.raised_deposit(
                            buy_amount,
                            destination_minimum,
                            current,
                            approved_allowance,
                        );
                        tracing::debug!(target: "executor_observation", step = "order_bridge_quote",
                        approved = %destination_minimum, current = %current,
                        within_cushion = deposit.is_some(), "below the approved minimum");
                        if let Some(deposit) = deposit
                            && let Ok(raised_buy) = review.require_approval(
                                deposit,
                                Some(destination_minimum),
                                price_acknowledged,
                            )
                        {
                            let raised_signing = trace_step(
                                "order_bridge_quote",
                                self.while_active(self.bridge_signing_terms(
                                    review,
                                    clients,
                                    leg,
                                    raised_buy,
                                    destination_minimum,
                                    valid_to,
                                    handler_message,
                                    &profile,
                                    anchor_cache,
                                    token_registry,
                                )),
                            )
                            .await?;
                            // A still-short quote for the raised deposit can't replace the review
                            // at its reviewed deposit. Keep that first change.
                            if matches!(raised_signing, BridgeSigning::Terms(_)) {
                                signing = raised_signing;
                                raised = Some((deposit, raised_buy));
                            }
                        }
                    }
                    match signing {
                        BridgeSigning::Terms(terms) => (Some(terms), private_delivery, raised),
                        BridgeSigning::Changed(change) => {
                            return Ok(SwapOrderOutcome::ReviewRequired(change));
                        }
                    }
                }
                None => (None, None, None),
            };
        // A raised deposit is the order's minimum and buy amount from here on.
        let (private_minimum, buy_amount, gas_allowance) = match raised {
            Some((deposit, raised_buy)) => {
                (deposit, raised_buy, review.gas_allowance_for(deposit)?)
            }
            None => (private_minimum, buy_amount, gas_allowance),
        };
        // A raised deposit can take back the reduction a speed-up was reviewed for.
        if let Some(change) =
            speed_up_open.and_then(|open| speed_up_buy_amount_change(open, buy_amount))
        {
            return Ok(SwapOrderOutcome::ReviewRequired(change));
        }
        // For a speed-up, an earlier attempt's post-hook above the nonce this pre-hook signs
        // at, the open order's included, can still deposit under its own lower minimum. The
        // user must have accepted that minimum. An ordinary retry discloses none.
        if plan.speed_up.is_some()
            && is_public_across(&plan.delivery)
            && let Some(destination_minimum) = destination_minimum
        {
            let continuing = trace_step(
                "order_continuing_approvals",
                self.swap_continuing_approvals(
                    operation,
                    swap_use,
                    &plan.delivery,
                    Some(nonce),
                    client,
                ),
            )
            .await?;
            if let Some(current) = continuing.lower_minimum(destination_minimum)
                && continuing_minimum.is_none_or(|accepted| accepted > current)
            {
                return Ok(SwapOrderOutcome::ReviewRequired(
                    SwapReviewChange::ContinuingApproval {
                        approved: continuing_minimum.unwrap_or_default(),
                        current,
                    },
                ));
            }
        }
        // The destination session is needed until the order is signed. One that ended during
        // the quote stops the order before anything of this account is signed.
        if let Some(destination) = destination {
            destination.owner.ensure_active()?;
        }
        // The open order may have expired while a quote was awaited. A new order can't relax
        // one that solvers no longer fill.
        if let Some(speed_up) = plan.speed_up {
            require_speed_up_window(
                &record,
                &profile,
                &speed_up,
                SystemTime::now(),
                delegated.inclusion(),
            )?;
        }
        // Resolved before anything is signed.
        let receiver = order_receiver(executor, &plan.delivery, bridge_terms.as_ref())?;

        let signing_started = Instant::now();
        tracing::debug!(target: "executor_observation", step = "order_sign", "started");
        let chain_id = self.chain.chain_id;
        let proof_digest = keccak256(transactions.abi_encode());
        let unsigned = TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: transactions,
                _actionData: RelayAdapt7702ActionData {
                    requireSuccess: true,
                    minGasLimit: U256::ZERO,
                    calls: pre_hook_calls(
                        &profile,
                        executor,
                        plan.sell_token,
                        review.sell_amount,
                        valid_to,
                        plan.invalidates,
                    )?,
                },
                _nonce: nonce,
                _signature: Bytes::new(),
            }
            .abi_encode()
            .into(),
        };
        let pre_hook_hash = plan.context.signing_hash(&unsigned)?;
        let signer = self.authorized_executor_signer(
            authorization,
            &HardwareExecutorAction::Execute(operation),
            operation,
            record.index(),
        )?;
        if signer.address() != executor {
            return Err(eyre!("executor signing identity does not match the swap"));
        }
        let pre_hook = plan
            .context
            .authorize_call(&unsigned, signer.sign_hash_sync(&pre_hook_hash)?)?
            .data;
        let recipient = self.view.scan_keys().address_data();
        // Every post-hook shield of this swap shields the full buy-token balance to the
        // wallet's own address.
        let wallet_shield = || self.wallet_shield(&signer, plan.buy_token);
        // A Reshield post-hook shields the bought token and an Across post-hook deposits it,
        // shielding any surplus when the user chose to. A private delivery's deposit pays the
        // destination chain's handler with the message that runs the destination account's
        // shield. External and NEAR Intents orders pay their receiver and sign none.
        let post_hook_calls = match (&plan.delivery, &bridge_terms) {
            (SwapDelivery::Reshield, _) => Some(guarded_shield_calls(
                executor,
                plan.buy_token,
                buy_amount,
                wallet_shield()?,
            )?),
            (SwapDelivery::Bridge(delivery), Some(BridgeOrderTerms::Across(terms))) => {
                let deposit = across_deposit(executor, *delivery, terms);
                let surplus_shield = (delivery.surplus == BridgeSurplus::Reshield)
                    .then(wallet_shield)
                    .transpose()?;
                Some(match private_delivery {
                    Some(private) => {
                        let calls = private_bridge_deposit_calls(
                            executor,
                            terms.spoke_pool,
                            deposit,
                            private,
                            surplus_shield,
                        )?;
                        // The plan sized the message from a placeholder of the destination
                        // account's payload.
                        let planned =
                            placeholder_post_hook_calls(executor, plan.buy_token, &plan.delivery)?;
                        if calls.abi_encode().len() != planned.abi_encode().len() {
                            return Err(eyre!(
                                "the swap's post-hook differs from the planned one; plan the swap again"
                            ));
                        }
                        calls
                    }
                    None => {
                        bridge_deposit_calls(executor, terms.spoke_pool, deposit, surplus_shield)?
                    }
                })
            }
            (
                SwapDelivery::External { .. }
                | SwapDelivery::Bridge(_)
                | SwapDelivery::NonEvmBridge(_),
                _,
            ) => None,
        };
        let post_hook = match post_hook_calls {
            Some(calls) => {
                let post_hook_nonce = nonce
                    .checked_add(U256::ONE)
                    .ok_or_else(|| eyre!("executor nonce is exhausted"))?;
                let gas_limit = plan
                    .post_hook_gas_limit()
                    .ok_or_else(|| eyre!("the swap's post-hook is unavailable"))?;
                let hash = post_hook_signing_hash(&calls, post_hook_nonce, chain_id, executor);
                let calldata = signed_post_hook_calldata(
                    calls,
                    post_hook_nonce,
                    chain_id,
                    executor,
                    &signer.sign_hash_sync(&hash)?,
                )?;
                Some((post_hook_nonce, hash, calldata, gas_limit))
            }
            None => None,
        };
        let app_data = swap_app_data(
            profile.app_code(),
            executor,
            pre_hook.clone(),
            plan.pre_hook_gas_limit(),
            post_hook
                .as_ref()
                .map(|(_, _, calldata, gas_limit)| (calldata.clone(), *gas_limit)),
        )?;
        if app_data.document.len() > plan.byte_budget {
            // The plan's estimate is an upper bound, so the plan is stale. Nothing is kept.
            return Ok(SwapOrderOutcome::Replan {
                byte_budget: swap_replan_budget(plan.byte_budget, app_data.document.len()),
                attempt_recorded: false,
            });
        }
        let order = swap_order(
            plan.sell_token,
            plan.buy_token,
            receiver,
            review.sell_amount,
            buy_amount,
            valid_to,
            app_data.hash,
        );
        let digest = order_digest(&order, chain_id, profile.settlement());
        let signature = eip712_order_signature(&signer.sign_hash_sync(&digest)?);
        drop(signer);
        let uid = OrderUid::new(digest, executor, valid_to);
        tracing::debug!(target: "executor_observation", step = "order_sign",
            elapsed_ms = signing_started.elapsed().as_millis(), "finished");

        let input_identities = inputs
            .iter()
            .map(ExecutorInputIdentity::from_utxo)
            .collect::<Vec<_>>();
        // The change outputs' PPOI contexts are committed before the attempt becomes durable,
        // so a failed commit leaves no attempt that must wait for expiry.
        if !change_output_pois.is_empty() {
            trace_step(
                "order_output_poi_commit",
                self.while_active(output_pois.commit(&change_output_pois)),
            )
            .await?;
        }
        self.ensure_active()?;
        if let Some(destination) = destination {
            destination.owner.ensure_active()?;
        }
        let guard = self.lock_activity().await;
        self.require_record_unchanged(&record)?;
        // An order on the included setup is admitted on the account read at the latest block
        // again, under the lock the write holds. When a reorg removed the setup since the
        // account was refreshed, nothing is written or sent.
        let inclusion = if delegated.inclusion().is_some() {
            let endpoints = self.observation_endpoints(client)?;
            let inclusion = trace_step(
                "order_included_setup",
                self.latest_setup_inclusion(&record, &endpoints),
            )
            .await?
            .filter(|inclusion| inclusion.nonce() == nonce)
            .ok_or(SwapAccountRefusal::SetupUnconfirmed)?;
            require_swap_account(
                &record,
                chain_id,
                SwapAccountRole::Source,
                account_use,
                inclusion.evidence(),
            )?;
            Some(inclusion)
        } else {
            None
        };
        // The open order may have expired while the lock was awaited. A new order can't relax
        // one that solvers no longer fill.
        if let Some(speed_up) = plan.speed_up {
            require_speed_up_window(&record, &profile, &speed_up, SystemTime::now(), inclusion)?;
        }
        trace_step("order_persist", async {
            let attempt = SwapAttempt {
                use_id: swap_use,
                terms: SwapTerms::new(
                    plan.sell_token,
                    plan.buy_token,
                    SwapRecipient::new(recipient.master_public_key, recipient.viewing_public_key),
                    delegated.setup_payload(),
                ),
                proof: SwapProof::new(proof_digest, input_identities.clone()),
                uid,
                submission: Some(SwapSubmission::new(signature, review.quote_id)),
                delivery: plan.delivery.clone(),
                bounds: SwapApprovedBounds {
                    sell_amount: review.sell_amount,
                    unshield_amount: Some(plan.amount()),
                    unshield_fee_bps: review.unshield_fee_bps,
                    buy_amount,
                    private_minimum,
                    shield_fee_bps: review.shield_fee_bps,
                    slippage_bps: review.slippage_bps,
                    pre_hook_gas_limit: plan.pre_hook_gas_limit(),
                    post_hook_gas_limit: post_hook.as_ref().map(|(_, _, _, gas_limit)| *gas_limit),
                    hook_cost: Some(review.gas_estimate()),
                    anchors,
                    destination_minimum,
                    gas_share_bps: Some(review.gas_share_bps),
                    gas_estimate: Some(review.gas_estimate()),
                    gas_allowance: Some(gas_allowance),
                    gas_price_wei: Some(review.gas.gas_price_wei),
                    valid_for_secs: Some(valid_for_secs),
                    destination_shield_fee_bps: private_quote
                        .map(|private| private.destination_shield_fee_bps),
                    delivery_allowance: private_quote.map(|private| private.delivery_allowance),
                    // The limits approved with the setup, when this swap's record has them.
                    destination_setup_fee: private_quote
                        .and_then(|_| record.swap_approval()?.bounds.destination_setup_fee),
                    source_setup_fee: private_quote
                        .and_then(|_| record.swap_approval()?.bounds.source_setup_fee),
                },
                invalidates: plan.invalidates,
                pre_hook: IssuedExecutorPayload::new(
                    nonce,
                    delegated.delegate(),
                    pre_hook_hash,
                    ExecutorPayloadPurpose::SwapPreHook,
                    ExecutorPayloadContext::new(pre_hook.clone(), observed, input_identities),
                ),
                post_hook: post_hook.map(|(post_hook_nonce, hash, calldata, _)| {
                    IssuedExecutorPayload::new(
                        post_hook_nonce,
                        delegated.delegate(),
                        hash,
                        ExecutorPayloadPurpose::SwapPostHook,
                        ExecutorPayloadContext::new(calldata, observed, Vec::new()),
                    )
                }),
                bridge: bridge_terms,
                speed_up: plan.speed_up.map(|speed_up| speed_up.order),
            };
            match inclusion {
                Some(inclusion) => self.store.record_swap_attempt_on_included_setup(
                    operation,
                    attempt,
                    inclusion.nonce(),
                ),
                None => self.store.record_swap_attempt(operation, attempt),
            }
        })
        .await?;
        self.notify_change();
        drop(guard);

        // The orderbook request carries the signed hooks, so it follows the durable write.
        let submitted = self
            .while_active(async {
                Ok(trace_step(
                    "order_submit",
                    orderbook.submit_order(&CowOrderSubmission {
                        order: &order,
                        owner: executor,
                        signature: &signature,
                        app_data: &app_data,
                        quote_id: review.quote_id,
                    }),
                )
                .await)
            })
            .await?;
        self.record_swap_submission_result(operation, uid, &submitted)?;
        self.diagnose_unfunded_rejection(
            &submitted,
            profile.hooks_trampoline(),
            executor,
            &pre_hook,
            plan.pre_hook_gas_limit(),
            client,
        )
        .await?;
        swap_submission_outcome(submitted, uid, app_data.document.len(), plan.byte_budget)
    }

    /// Sign and persist the guarded shield of this chain's destination stealth account for a
    /// private Bridge order, at the nonce `delegated` was confirmed with: revert unless the
    /// account holds `amount` of `token`, the deposit's signed output amount, then shield its
    /// full balance to the wallet. The guard makes a submission before the fill revert without
    /// consuming the nonce. The payload is in the account's record when its calldata is
    /// returned. A retry signs again at the same nonce for its own amount, and both stay
    /// recorded. The shield is signed for the swap use `swap_use`, which must claim the account
    /// as its destination for `token`, also when the payload is written.
    ///
    /// An account the use reuses is judged again before each shield, the first and every
    /// retry's: the shared admission rules on its reconciled record, a zero balance of `token`
    /// read now, and, in the write's own critical section, the POI verdicts of its earlier
    /// shields from `notes`. A verdict that changed since admission issues nothing.
    pub(crate) async fn issue_swap_destination_shield(
        &self,
        delegated: DelegatedSwapExecutor,
        swap_use: SwapUseId,
        token: Address,
        amount: U256,
        authorization: &DesktopPrivateSpendAuthorization,
        notes: Option<&dyn SwapShieldNotes>,
        client: &OperationHttpClient,
    ) -> Result<Bytes> {
        self.ensure_active()?;
        let (operation, executor) = (delegated.operation(), delegated.executor());
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        // Like the swap's own hooks, the shield is issued against the latest reconciled
        // observation, which the write below requires to be unchanged, at the nonce the
        // delegation was confirmed with.
        let observed = record
            .nonce_observation()
            .filter(|observed| observed.nonce() == delegated.observed().nonce())
            .ok_or_else(|| {
                eyre!("the destination stealth account's nonce changed; plan the swap again")
            })?;
        let receives_token = record.swap_use(swap_use).is_some_and(|claimed| {
            matches!(
                claimed.role(),
                SwapUseRole::Destination { destination_token, .. }
                    | SwapUseRole::PublicSourceDestination { destination_token, .. }
                    if *destination_token == token
            )
        });
        if record.address() != Some(executor)
            || record.delegate() != delegated.delegate()
            || !is_live_swap_use(&record, swap_use)
            || !receives_token
        {
            return Err(eyre!(
                "the destination stealth account changed; plan the swap again"
            ));
        }
        let reused = record
            .swap_use(swap_use)
            .is_some_and(|claimed| !claimed.is_fresh());
        if reused {
            require_swap_account(
                &record,
                self.chain.chain_id,
                SwapAccountRole::Destination { token },
                SwapAccountUse::Claimed(swap_use),
                SwapAdmissionEvidence::Fresh,
            )?;
            self.require_empty_receiving_balance(executor, delegated.delegate(), token, client)
                .await?;
        }
        let signer = self.authorized_executor_signer(
            authorization,
            &HardwareExecutorAction::Execute(operation),
            operation,
            record.index(),
        )?;
        if signer.address() != executor {
            return Err(eyre!(
                "executor signing identity does not match the destination stealth account"
            ));
        }
        let (nonce, chain_id) = (observed.nonce(), self.chain.chain_id);
        let calls =
            guarded_shield_calls(executor, token, amount, self.wallet_shield(&signer, token)?)?;
        let hash = post_hook_signing_hash(&calls, nonce, chain_id, executor);
        let calldata = signed_post_hook_calldata(
            calls,
            nonce,
            chain_id,
            executor,
            &signer.sign_hash_sync(&hash)?,
        )?;
        drop(signer);
        let guard = self.lock_activity().await;
        self.require_record_unchanged(&record)?;
        if reused {
            admission::require_earlier_shields_resolved(&record, swap_use, notes)?;
        }
        self.store.record_swap_destination_shield(
            operation,
            swap_use,
            IssuedExecutorPayload::new(
                nonce,
                delegated.delegate(),
                hash,
                ExecutorPayloadPurpose::SwapDestinationShield,
                ExecutorPayloadContext::new(calldata.clone(), observed, Vec::new()),
            ),
        )?;
        self.notify_change();
        drop(guard);
        Ok(calldata)
    }

    /// A shield of an executor's full `token` balance to the wallet's own address on this
    /// chain, keyed from that executor's `signer` like executor recovery's shields.
    fn wallet_shield(&self, signer: &PrivateKeySigner, token: Address) -> Result<ShieldRequest> {
        let recipient = self.view.scan_keys().address_data();
        let shield_key = Zeroizing::new(derive_shield_private_key(&Zeroizing::new(
            signer.to_bytes().0,
        ))?);
        Ok(build_shield_request(
            recipient.master_public_key,
            &recipient.viewing_public_key,
            TokenData::erc20(token),
            U120::ZERO,
            &shield_key,
        )?)
    }

    /// Check the pair's cached anchors right before signing. Railgun's fees are the shared
    /// protocol constant, as for every other operation, so nothing is read from the chain. An
    /// External order depends only on the unshield fee.
    fn recheck_swap_review(
        &self,
        review: &SwapReview,
        profile: &SwapProfile,
        anchor_cache: &TokenAnchorRateCache,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<SwapRecheck> {
        let plan = &review.plan;
        let started = Instant::now();
        let anchor = anchor_cache
            .cached_pair_rate(
                self.chain.chain_id,
                plan.sell_token,
                anchor_token(self.chain.chain_id, plan.buy_token),
                token_registry,
            )
            .ok()
            .flatten();
        tracing::debug!(target: "executor_observation", step = "order_price_anchors",
            source = "cache", elapsed_ms = started.elapsed().as_millis(),
            available = anchor.is_some(), "finished");
        recheck_swap_price(
            &review.quote,
            &review.price,
            anchor,
            profile.anchor_deviation_bps(),
        )
    }
}
