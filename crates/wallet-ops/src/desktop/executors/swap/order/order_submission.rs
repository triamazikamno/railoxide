use super::*;

impl ExecutorOwner {
    /// Prove the planned pre-hook, or reuse the recorded proof when a retry spends the same
    /// notes for the same amount, then sign, persist, and submit the order.
    pub async fn submit_swap_order(
        &self,
        request: SwapOrderRequest<'_>,
    ) -> Result<SwapOrderOutcome> {
        static NEXT_PREPARATION: AtomicU64 = AtomicU64::new(1);
        let span = tracing::debug_span!(target: "executor_observation", "swap_order",
            preparation_id = NEXT_PREPARATION.fetch_add(1, Ordering::Relaxed));
        trace_step(
            "order_total",
            self.while_active(Box::pin(self.submit_swap_order_active(request))),
        )
        .instrument(span)
        .await
    }

    async fn submit_swap_order_active(
        &self,
        request: SwapOrderRequest<'_>,
    ) -> Result<SwapOrderOutcome> {
        let SwapOrderRequest {
            review,
            swap_use,
            private_minimum,
            price_acknowledged,
            session,
            authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
            destination,
            continuing_minimum,
            verify_proof,
        } = request;
        self.require_swap_session(&session)?;
        // The orderbook's route is the swap's, which every account read of the order shares.
        let client = orderbook.http();
        review.require_approval(private_minimum, destination_minimum, price_acknowledged)?;
        bridge_route(&review.plan, bridge)?;
        if review.plan.delivery.private_bridge().is_some() != destination.is_some() {
            return Err(eyre!(DESTINATION_ACCOUNT_MISMATCH));
        }
        let account_use = order_account_use(&review.plan, swap_use)?;
        // The use claims its account before the account is read or anything is proved.
        self.admit_swap_use(review, swap_use, private_minimum, price_acknowledged)?;
        let confirmed = session
            .sync_tip_rx
            .borrow()
            .safe_head_block
            .ok_or_else(|| eyre!("waiting for the wallet to sync the chain head"))?;
        let operation = review
            .plan
            .operation()
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        // A private Bridge order is signed only once the destination chain's stealth account is
        // confirmed as delegated too, through that chain's owner. The swap's record names it.
        let destination_account = match (&destination, review.plan.delivery.private_bridge()) {
            (Some(context), Some(delivery)) => {
                let destination_operation = self
                    .swap_account_record(operation)?
                    .and_then(|record| record.destination_operation())
                    .ok_or_else(|| {
                        eyre!("this swap has no stealth account on the destination network")
                    })?;
                context.owner.require_swap_session(&context.session)?;
                let confirmed = context
                    .session
                    .sync_tip_rx
                    .borrow()
                    .safe_head_block
                    .ok_or_else(|| {
                        eyre!("waiting for the wallet to sync the destination network")
                    })?;
                Some((context, destination_operation, confirmed, delivery.receiver))
            }
            _ => None,
        };
        // Both selected accounts are refreshed at the same time, each through its own owner
        // and session. Each result is bound to its record, which signing checks again.
        let mut review = review.clone();
        let (source, destination) = tokio::join!(
            trace_step(
                "order_account_refresh",
                self.refresh_swap_executor(&mut review, confirmed, account_use, client),
            ),
            async {
                let Some((context, destination_operation, confirmed, receiver)) =
                    destination_account
                else {
                    return Ok(None);
                };
                let delegated = trace_step(
                    "order_destination_refresh",
                    context.owner.delegated_swap_destination(
                        destination_operation,
                        confirmed,
                        self.chain.chain_id,
                        operation,
                        swap_use,
                        receiver,
                        Some(&*context.session),
                        client,
                    ),
                )
                .await?;
                Ok::<_, eyre::Report>(Some(SwapDestinationSigning {
                    owner: context.owner.as_ref(),
                    delegated,
                    authorization: &context.authorization,
                    notes: Some(&*context.session),
                }))
            },
        );
        if let Some(change) = source? {
            return Ok(SwapOrderOutcome::ReviewRequired(change));
        }
        let destination = destination?;
        let review = &review;
        let plan = &review.plan;
        let delegated = require_delegated_plan(plan)?;
        let record = self
            .swap_account_record(delegated.operation())?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        // No proof is built for a retry the store would refuse.
        require_swap_account(
            &record,
            self.chain.chain_id,
            SwapAccountRole::Source,
            account_use,
            delegated.evidence(),
        )?;
        require_swap_use_terms(&record, swap_use, plan)?;
        let utxos = self.swap_inputs(&session, &record)?;
        let (transactions, inputs, change_output_pois) = if let Some((transactions, inputs)) =
            reusable_swap_proof(&record, plan, &utxos)
        {
            tracing::debug!(target: "executor_observation", step = "order_proof", reused = true);
            (transactions, inputs, Vec::new())
        } else if plan.speed_up.is_some() {
            // A speed-up funds from the recorded proof alone, so nothing is proved for it.
            return Err(eyre!(SPEED_UP_FUNDS_CHANGED));
        } else {
            trace_step(
                "order_proof",
                Box::pin(self.prove_swap_pre_hook(
                    plan,
                    &session,
                    &authorization,
                    &utxos,
                    verify_proof,
                )),
            )
            .await?
        };
        Box::pin(self.issue_swap_order(SwapOrderSigning {
            review,
            swap_use,
            private_minimum,
            price_acknowledged,
            transactions,
            inputs: &inputs,
            change_output_pois,
            output_pois: session.as_ref(),
            authorization: &authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
            destination,
            continuing_minimum,
        }))
        .await
    }

    /// Bind the order to its swap use before any preparation. An explicitly reused account
    /// that doesn't hold `swap_use` yet is claimed for it, with the approval these terms make.
    /// Any other account must already be claimed by that use. A speed-up never claims one.
    fn admit_swap_use(
        &self,
        review: &SwapReview,
        swap_use: SwapUseId,
        private_minimum: U256,
        price_acknowledged: bool,
    ) -> Result<()> {
        let operation = review
            .plan
            .operation()
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        if review.plan.speed_up.is_none()
            && review.plan.executor.is_reused()
            && record.swap_use(swap_use).is_none()
        {
            self.claim_swap_use(
                None,
                SwapUseClaim {
                    id: swap_use,
                    source: SwapAccountChoice::Existing(operation),
                    approval: review.approval(private_minimum, price_acknowledged)?,
                    destination: None,
                },
            )?;
            return Ok(());
        }
        require_swap_use(&record, swap_use)
    }

    async fn prove_swap_pre_hook(
        &self,
        plan: &SwapInputPlan,
        session: &WalletSession,
        authorization: &DesktopPrivateSpendAuthorization,
        utxos: &[Utxo],
        verify_proof: bool,
    ) -> Result<(
        Vec<Transaction>,
        Vec<Utxo>,
        Vec<PendingOutputPoiContextRecord>,
    )> {
        let profile = self.swap_order_profile()?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
        };
        let mut request = plan.proof_request(&profile)?;
        request.verify_proof = verify_proof;
        let source = artifact_source(&self.http, &session.db)?;
        let prover = session.prover_service(&source);
        let chain_handle = session
            .sync_manager
            .chain_handle(&session.chain_key)
            .await
            .ok_or_else(|| eyre!("swap chain is unavailable"))?;
        let mut forest = trace_step("order_forest", async {
            Ok::<_, eyre::Report>(chain_handle.forest.read().await.clone())
        })
        .await?;
        forest.compute_roots();
        let signer = authorization.signer(&self.vault, &self.view, SWAP_SPEND_OPERATION)?;
        let proved = trace_step(
            "order_transaction_proof",
            builder.build_mixed_private_action_plan_with_signer(
                &self.view.scan_keys(),
                &signer,
                &forest,
                utxos,
                request,
                &prover,
            ),
        )
        .await
        .map_err(|error| match error {
            BuildError::PinnedInputUnavailable { .. }
            | BuildError::PinnedInputsChanged { .. }
            | BuildError::PinnedInputsInsufficient(_) => {
                eyre!("The funds selected for this swap changed. Review the quote again.")
            }
            BuildError::CompositePlanShapeChanged { .. } => {
                eyre!("The swap changed during preparation. Review the quote again.")
            }
            error => error.into(),
        })?;
        drop(signer);
        // Like any private spend, the pre-hook's change outputs need sender-output PPOI. Only
        // change is private; the sell amount is unshielded to the executor.
        let (poi_list_keys, pre_transaction_pois) = trace_step(
            "order_pre_transaction_poi",
            active_list_pre_transaction_pois(
                &proved.chunks,
                session,
                self.chain.chain_id,
                &prover,
                verify_proof,
                &self.http,
                "generate private swap pending output pre-transaction POI",
            ),
        )
        .await?;
        let change = proved
            .private_outputs
            .into_iter()
            .filter(|output| output.role == MixedPrivateOutputRole::Change)
            .collect::<Vec<_>>();
        let change_output_pois = build_pending_mixed_output_poi_context_records(
            self.chain.chain_id,
            &session.cache_key,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            &proved.chunks,
            &change,
            &pre_transaction_pois,
            &poi_list_keys,
        )?;
        let transactions =
            RelayAdapt7702::executeCall::abi_decode(&proved.call.data)?._transactions;
        Ok((
            transactions,
            proved.inputs.into_iter().map(|input| input.utxo).collect(),
            change_output_pois,
        ))
    }

    /// Resend the original signed order without signing or authorizing new terms.
    /// Use this swap's isolated client, retained by the view or created after restart.
    pub async fn resubmit_swap_order(
        &self,
        operation: ExecutorOperationId,
        orderbook: &CowOrderbookClient,
    ) -> Result<SwapOrderOutcome> {
        let guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let swap = record
            .swap()
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        let saved = swap
            .orders()
            .last()
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        if saved.submission_status() == SwapSubmissionStatus::Accepted {
            return Ok(SwapOrderOutcome::Submitted { uid: saved.uid() });
        }
        if saved.submission_status() == SwapSubmissionStatus::Rejected {
            return Err(eyre!(
                "the order was rejected; wait for its attempt to end before retrying"
            ));
        }
        if !matches!(
            crate::desktop::executors::swap::observation::swap_order_state(swap, saved),
            crate::desktop::executors::swap::observation::SwapOrderState::Open
                | crate::desktop::executors::swap::observation::SwapOrderState::PreHookOnly {
                    expired: false
                }
        ) {
            return Err(eyre!("this order no longer awaits submission"));
        }
        // Leave at least one request timeout for the existing order to be accepted.
        if saved.valid_to() <= valid_to_after(SystemTime::now(), Duration::from_mins(1))? {
            return Err(eyre!(
                "the order is too close to expiry; wait for final expiry, then retry"
            ));
        }
        let submission = saved.submission().ok_or_else(|| {
            eyre!("this saved order has no resubmission signature; wait for expiry")
        })?;
        let profile = self.swap_order_profile()?;
        let executor = saved.uid().owner();
        let payload = |hash| {
            record
                .issued()
                .iter()
                .find(|payload| payload.hash() == hash)
                .map(|payload| payload.context().calldata().clone())
                .ok_or_else(|| eyre!("the saved swap hook is unavailable"))
        };
        let bounds = saved.bounds();
        let pre_hook = payload(saved.pre_hook().payload())?;
        let post_hook = match saved.post_hook() {
            Some(post_hook) => Some((
                payload(post_hook.payload())?,
                bounds
                    .post_hook_gas_limit
                    .ok_or_else(|| eyre!("the saved swap hook is unavailable"))?,
            )),
            None => None,
        };
        let app_data = swap_app_data(
            profile.app_code(),
            executor,
            pre_hook.clone(),
            bounds.pre_hook_gas_limit,
            post_hook,
        )?;
        let order = swap_order(
            swap.order_terms(saved).sell_token(),
            swap.order_terms(saved).buy_token(),
            order_receiver(executor, &saved.delivery(), saved.bridge())?,
            bounds.sell_amount,
            bounds.buy_amount,
            saved.valid_to(),
            app_data.hash,
        );
        let uid = OrderUid::new(
            order_digest(&order, self.chain.chain_id, profile.settlement()),
            executor,
            saved.valid_to(),
        );
        if uid != saved.uid() {
            return Err(eyre!(
                "the saved order can't be reconstructed unchanged; wait for expiry"
            ));
        }
        // Sending the order again reserves notes the user released from it.
        match self
            .store
            .reserve_released(operation, saved.pre_hook().payload())
        {
            Ok(_) => {}
            Err(ExecutorStoreError::InputReserved) => {
                return Err(eyre!(
                    "another operation now uses this order's notes; wait for the order to expire"
                ));
            }
            Err(error) => return Err(error.into()),
        }
        self.notify_change();
        drop(guard);
        let result = self
            .while_active(async {
                Ok(orderbook
                    .submit_order(&CowOrderSubmission {
                        order: &order,
                        owner: executor,
                        signature: submission.signature(),
                        app_data: &app_data,
                        quote_id: submission.quote_id(),
                    })
                    .await)
            })
            .await?;
        self.record_swap_submission_result(operation, uid, &result)?;
        self.diagnose_unfunded_rejection(
            &result,
            profile.hooks_trampoline(),
            executor,
            &pre_hook,
            bounds.pre_hook_gas_limit,
            orderbook.http(),
        )
        .await?;
        swap_submission_outcome(
            result,
            uid,
            app_data.document.len(),
            app_data.document.len(),
        )
    }

    /// `CoW`'s trampoline ignores a reverting pre-hook, so the orderbook reports the account
    /// as unfunded without the reason. Only after that rejection, simulate the pre-hook to
    /// report why. Any other result, or a simulation without a failure, is left to the caller.
    pub(super) async fn diagnose_unfunded_rejection(
        &self,
        result: &Result<OrderUid, CowApiError>,
        trampoline: Address,
        executor: Address,
        pre_hook: &Bytes,
        gas_limit: u64,
        client: &OperationHttpClient,
    ) -> Result<()> {
        let Err(CowApiError::Rejected { error_type, .. }) = result else {
            return Ok(());
        };
        if !matches!(
            error_type.as_str(),
            "InsufficientBalance" | "InsufficientAllowance"
        ) {
            return Ok(());
        }
        let endpoints = self.observation_endpoints(client)?;
        let simulation = trace_step(
            "order_pre_hook_simulation",
            self.while_active(async {
                Ok(simulate_pre_hook(
                    &endpoints,
                    trampoline,
                    executor,
                    pre_hook,
                    gas_limit,
                    self.chain.chain_id,
                )
                .await)
            }),
        )
        .await?;
        if let PreHookSimulation::Failed(reason) = simulation {
            return Err(eyre!(
                "CoW rejected the order because its pre-hook fails: {reason}"
            ));
        }
        Ok(())
    }

    pub(super) fn record_swap_submission_result(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        result: &Result<OrderUid, CowApiError>,
    ) -> Result<()> {
        let status = swap_submission_status(result, uid);
        self.ensure_active()?;
        self.store.record_swap_submission(operation, uid, status)?;
        self.notify_change();
        Ok(())
    }
}
