use super::*;

impl ExecutorOwner {
    /// An orderbook client on `client`, the network route dedicated to one swap. Keep it for
    /// that swap's quotes and its order submission, and read the swap's accounts over the same
    /// client.
    pub fn swap_orderbook_client(
        &self,
        client: &OperationHttpClient,
    ) -> Result<CowOrderbookClient> {
        self.ensure_active()?;
        let profile = self.swap_order_profile()?;
        let base_url = Url::parse(profile.orderbook_api_base())?;
        Ok(CowOrderbookClient::new(
            client.clone(),
            base_url,
            self.chain.chain_id,
        )?)
    }

    /// `orderbook` and `bridge` on `client`, a new network route of the swap's own, for an
    /// explicit status check, so a route its provider rejects isn't asked again. Other swaps
    /// and the shared session are untouched.
    pub fn swap_clients_on_fresh_route(
        &self,
        orderbook: &CowOrderbookClient,
        bridge: &SwapBridgeClients,
        client: &OperationHttpClient,
    ) -> Result<(CowOrderbookClient, SwapBridgeClients)> {
        self.ensure_active()?;
        Ok((orderbook.on_route(client.clone()), bridge.on_route(client)))
    }

    /// Plan the pre-hook's notes for `request` from POI-spendable notes that no other executor
    /// operation reserves. Nothing is proved or signed. An amount that doesn't fit yields the
    /// largest amount that does, to offer instead. `executor` may be a preview, a reserved
    /// executor whose setup isn't confirmed, or the delegated one; only a plan for the
    /// delegated executor can be signed.
    pub fn plan_swap_amount(
        &self,
        executor: &SwapExecutor,
        session: &WalletSession,
        request: &SwapAmountRequest,
    ) -> Result<SwapAmountPlan> {
        self.require_swap_session(session)?;
        let profile = self.swap_order_profile()?;
        if profile.pair_eligibility(request.sell_token, request.buy_token, &request.delivery)
            != SwapTokenEligibility::Eligible
        {
            return Err(eyre!("this token pair is not eligible for private swaps"));
        }
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
        };
        let profile_budget = profile.app_data_byte_budget();
        let byte_budget = request
            .byte_budget
            .map_or(profile_budget, |budget| budget.min(profile_budget));
        let Some(operation) = executor.operation() else {
            // A preview spends only notes that no executor operation reserves.
            let utxos = self.filter_reserved_inputs(self.spendable_swap_notes(session)?, None)?;
            return plan_swap_inputs(
                &builder,
                &profile,
                *executor,
                &utxos,
                request,
                byte_budget,
                None,
            );
        };
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        // Only a plan made for the confirmed or included delegation has reconciled evidence
        // behind it.
        let evidence = executor
            .delegated()
            .map_or(SwapAdmissionEvidence::Recorded, |delegated| {
                delegated.evidence()
            });
        require_swap_account(
            &record,
            self.chain.chain_id,
            SwapAccountRole::Source,
            previewed_swap_use(&record, executor.is_reused()),
            evidence,
        )?;
        // Without reuse, the executor places the pair it was set up for: the setup approval's
        // before the first order, then the last order's.
        let pair = match record.swap() {
            Some(swap) => Some((swap.terms().sell_token(), swap.terms().buy_token())),
            None => record.swap_approval_tokens(),
        };
        if record.address() != Some(executor.executor())
            || record.delegate() != executor.delegate()
            || !executor.is_reused() && pair != Some((request.sell_token, request.buy_token))
        {
            return Err(eyre!("this swap's executor was set up for other tokens"));
        }
        let utxos = self.swap_inputs(session, &record)?;
        plan_swap_inputs(
            &builder,
            &profile,
            *executor,
            &utxos,
            request,
            byte_budget,
            swap_invalidation(&record, &profile, SystemTime::now())?,
        )
    }

    /// Plan a second order for the open order `order` of the swap `operation`, from local
    /// records only. Nothing is proved or signed. The plan keeps the open order's tokens,
    /// private spend and delivery, spends exactly the notes of the recorded proof, invalidates
    /// the open order and names it, so review and signing stay bound to it. Like a recorded
    /// preview it can't be signed until execution preparation confirms the account at the open
    /// order's pre-hook nonce. While the record leaves the swap's own setup unresolved, the
    /// speed-up needs `inclusion`, the session's last read at the latest block that showed the
    /// setup included.
    pub fn plan_swap_speed_up(
        &self,
        session: &WalletSession,
        operation: ExecutorOperationId,
        order: OrderUid,
        inclusion: Option<SwapSetupInclusion>,
    ) -> Result<SwapAmountPlan> {
        self.require_swap_session(session)?;
        let profile = self.swap_order_profile()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let (_, speed_up) =
            swap_speed_up_binding(&record, order).ok_or_else(|| eyre!(SPEED_UP_ORDER_CHANGED))?;
        let executor =
            recorded_swap_executor(&record, self.chain.chain_id, speed_up.pre_hook_nonce, false)?;
        let utxos = self.swap_inputs(session, &record)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
        };
        plan_swap_speed_up_inputs(
            &builder,
            &profile,
            &record,
            executor,
            &utxos,
            order,
            SystemTime::now(),
            inclusion,
        )
    }

    /// Why the order `order` of the swap `operation` can't be sped up now, judged from the
    /// record alone and without a network request. `None` is provisional: it offers the
    /// action, and planning and signing check the order again. While the record leaves the
    /// swap's own setup unresolved, only `inclusion`, the session's last read at the latest
    /// block that showed the setup included, offers it.
    pub fn swap_speed_up_refusal(
        &self,
        operation: ExecutorOperationId,
        order: OrderUid,
        inclusion: Option<SwapSetupInclusion>,
    ) -> Result<Option<SwapSpeedUpRefusal>> {
        let profile = self.swap_order_profile()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let chain_id = self.chain.chain_id;
        let admitted = speed_up_block(
            &record,
            &profile,
            chain_id,
            order,
            SystemTime::now(),
            inclusion,
        );
        Ok(match admitted {
            Ok(_) => None,
            Err(SpeedUpBlock::TwoOrdersFillable) => Some(SwapSpeedUpRefusal::TwoOrdersFillable),
            Err(_) => Some(SwapSpeedUpRefusal::Unavailable),
        })
    }

    /// Quote using the shared protocol fee and background anchor rates, without hooks, and
    /// price the order limit. Signing rechecks the fees and anchors before using these terms.
    pub async fn review_swap(&self, request: SwapReviewRequest<'_>) -> Result<SwapReview> {
        self.while_active(Box::pin(self.review_swap_active(request)))
            .await
    }

    async fn review_swap_active(&self, request: SwapReviewRequest<'_>) -> Result<SwapReview> {
        let SwapReviewRequest {
            plan,
            slippage_bps,
            gas_share_bps,
            valid_for,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
        } = request;
        let profile = self.swap_order_profile()?;
        let bridge = bridge_route(&plan, bridge)?;
        // A bridge leg is quoted for the profile's window.
        let valid_for = if plan.delivery.is_bridge() {
            profile.valid_to_window()
        } else {
            valid_for
        };
        if let Some(operation) = plan.operation() {
            self.swap_account_record(operation)?
                .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        }
        let unshield_fee_bps = RAILGUN_PROTOCOL_FEE_BPS;
        let shield_fee_bps = RAILGUN_PROTOCOL_FEE_BPS;
        let sell_amount = order_sell_amount(plan.amount(), unshield_fee_bps)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        // The gas price and data cost reads share this quote's candidates.
        let fee_candidates = acquire_fee_candidates(
            &self.http,
            &chain.rpc_route.for_class(crate::RpcTrafficClass::General)?,
            None,
        )
        .await?;
        // Before setup the quote names the reserved executor or, for a preview, a stand-in
        // with no code, just as a fresh executor has none.
        let quote = async {
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "cow_quote", "started");
            let quote = orderbook
                .quote_sell(&swap_quote_request(
                    &plan,
                    sell_amount,
                    valid_to_after(SystemTime::now(), valid_for)?,
                ))
                .await;
            tracing::debug!(
                target: "swap_quote",
                step = "cow_quote",
                elapsed_ms = started.elapsed().as_millis(),
                success = quote.is_ok(),
                "finished"
            );
            quote.map_err(eyre::Report::from)
        };
        let gas_price = async {
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "gas_price", "started");
            let gas_price = gas_price_from_candidates_with_policy(&fee_candidates, 1, 1).await;
            tracing::debug!(
                target: "swap_quote",
                step = "gas_price",
                elapsed_ms = started.elapsed().as_millis(),
                success = gas_price.is_ok(),
                "finished"
            );
            gas_price
        };
        let (quote, gas_price_wei, hook_data_cost_wei) = tokio::try_join!(
            quote,
            gas_price,
            hook_data_cost_from_candidates(
                &fee_candidates,
                self.chain.chain_id,
                plan.app_data_len(),
                &chain.gas,
            ),
        )?;
        let started = Instant::now();
        tracing::debug!(target: "swap_quote", step = "price_anchors", "started");
        let anchor = anchor_cache.map_or(Ok(None), |cache| {
            cache.cached_pair_rate(
                self.chain.chain_id,
                plan.sell_token,
                anchor_token(self.chain.chain_id, plan.buy_token),
                token_registry,
            )
        });
        tracing::debug!(
            target: "swap_quote",
            step = "price_anchors",
            source = "cache",
            elapsed_ms = started.elapsed().as_millis(),
            status = match &anchor {
                Ok(Some(_)) => "available",
                Ok(None) => "unverified",
                Err(_) => "unavailable",
            },
            "finished"
        );
        let diagnostics = SwapQuoteDiagnostics::new(
            self.chain.chain_id,
            &quote.quote,
            SwapQuoteAnchor::from_cached(&anchor),
        );
        let price = match anchor {
            Ok(Some(rate)) => {
                check_quote_against_anchor(
                    quote.quote.sell_amount,
                    quote.quote.buy_amount,
                    rate,
                    profile.anchor_deviation_bps(),
                )?;
                SwapPrice::Verified {
                    rate,
                    observations: Vec::new(),
                }
            }
            Ok(None) | Err(_) => SwapPrice::Unverified,
        };
        let mut review = price_swap_review(
            plan,
            quote,
            price,
            shield_fee_bps,
            unshield_fee_bps,
            slippage_bps,
            gas_share_bps,
            valid_for,
            gas_price_wei,
            hook_data_cost_wei,
            orderbook.isolation(),
        )
        .map_err(|error| wrap_pricing_error(error, diagnostics))?;
        if let Some((clients, leg)) = bridge {
            // Bridge orders carry no shield, so the buy amount is the suggested minimum.
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "bridge_quote", "started");
            let quote = self
                .quote_swap_bridge(
                    clients,
                    &leg,
                    &profile,
                    review.limit.buy_amount,
                    slippage_bps,
                    anchor_cache,
                    token_registry,
                )
                .await;
            tracing::debug!(
                target: "swap_quote",
                step = "bridge_quote",
                elapsed_ms = started.elapsed().as_millis(),
                success = quote.is_ok(),
                "finished"
            );
            review.bridge = Some(quote?);
        }
        // A speed-up of a public Across swap leaves its open order live, so that order or an
        // earlier attempt's deposit can still decide its delivery. An ordinary retry's earlier
        // orders can't fill, so it discloses none. An explicitly reused account starts a new
        // use, whose attempts are compared with nothing. A speed-up names its use and signs at
        // its open order's nonce.
        if let Some(speed_up) = review.plan.speed_up
            && is_public_across(&review.plan.delivery)
            && !review.plan.executor.is_reused()
            && let Some(operation) = review.plan.operation()
        {
            review.continuing = self
                .swap_continuing_approvals(
                    operation,
                    speed_up.swap_use,
                    &review.plan.delivery,
                    Some(speed_up.pre_hook_nonce),
                    orderbook.http(),
                )
                .await?;
        }
        Ok(review)
    }

    /// `review` at another gas share, with its bridge leg quoted again on `route` for the new
    /// order buy amount. The `CoW` quote, the gas inputs, the plan and the validity stay as they
    /// were reviewed, and the orderbook isn't asked.
    pub async fn requote_swap_bridge(
        &self,
        review: &SwapReview,
        gas_share_bps: u16,
        route: SwapBridgeRoute<'_>,
        anchor_cache: Option<&TokenAnchorRateCache>,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<SwapReview> {
        self.while_active(Box::pin(async {
            let profile = self.swap_order_profile()?;
            let Some((clients, leg)) = bridge_route(&review.plan, Some(route))? else {
                return Err(eyre!("only a bridge swap has a bridge leg to quote"));
            };
            if let Some(operation) = review.plan.operation() {
                self.swap_account_record(operation)?
                    .ok_or_else(|| eyre!("swap executor is unavailable"))?;
            }
            let mut repriced = review.with_gas_share(gas_share_bps)?;
            // Bridge orders carry no shield, so the buy amount is the suggested minimum.
            repriced.bridge = Some(
                self.quote_swap_bridge(
                    clients,
                    &leg,
                    &profile,
                    repriced.limit.buy_amount,
                    repriced.slippage_bps,
                    anchor_cache,
                    token_registry,
                )
                .await?,
            );
            Ok(repriced)
        }))
        .await
    }
}
