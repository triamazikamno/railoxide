use super::*;

impl ExecutorOwner {
    /// Read what a withdrawal of the proceeds the swap's proxy holds would send, and plan its
    /// gas at the given fee. The amount is the proxy's whole balance of the bought token now,
    /// and a zero balance is an error. Allowed while the proxy holds the proceeds, before or
    /// after the order's hook batch can no longer run, and for a swap that was stopped.
    pub async fn review_public_swap_withdrawal(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        client: &OperationHttpClient,
    ) -> Result<PublicSwapWithdrawalReview> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let (proxy, token) = held_proceeds(&claimed.swap)?;
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(eyre!(
                "the withdrawal's priority fee is above its maximum fee"
            ));
        }
        let route = origin
            .rpc_route
            .for_class(crate::RpcTrafficClass::Sensitive)?
            .isolated(client.clone());
        let amount = self
            .while_active(query_erc20_balance(&route, &self.http, token, proxy))
            .await
            .wrap_err("read the proxy's balance of the bought token")?;
        if amount.is_zero() {
            return Err(eyre!("the proxy holds none of the token"));
        }
        let proxy_deployed = self
            .while_active(proxy_deployed(&route, &self.http, proxy))
            .await?;
        let gas_units = if proxy_deployed {
            PUBLIC_PROXY_WITHDRAWAL_GAS_UNITS
        } else {
            PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS
        };
        let overflow = || eyre!("the withdrawal's gas plan overflows");
        let gas_limit = gas_units
            .checked_add(origin.gas.gas_limit_buffer)
            .ok_or_else(overflow)?;
        let max_gas_cost = U256::from(gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .ok_or_else(overflow)?;
        Ok(PublicSwapWithdrawalReview {
            proxy,
            token,
            amount,
            proxy_deployed,
            gas_limit,
            max_gas_cost,
        })
    }

    /// Return the bought token from the proxy to the Public account: one transaction from that
    /// account through the factory.
    pub async fn submit_public_swap_withdrawal(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        review: &PublicSwapWithdrawalReview,
        hash_fallback_confirmed: bool,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_withdrawal_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            review,
            hash_fallback_confirmed,
            client,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_withdrawal`] with the Public account's signer.
    ///
    /// The account signs a batch of its own with one call, the token's transfer of the reviewed
    /// amount to the account, under a fresh nonce, and sends it through the factory, which
    /// deploys the proxy when it has no code. A call straight to a proxy without code would
    /// succeed and move nothing. The order's hook batch is left as it was signed. A withdrawal
    /// that loses the race to a late run of that batch reverts.
    pub(crate) async fn submit_public_swap_withdrawal_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        review: &PublicSwapWithdrawalReview,
        hash_fallback_confirmed: bool,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_signer(signer, &claimed)?;
        let (proxy, token) = held_proceeds(&claimed.swap)?;
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!("swaps from a Public account are unavailable on this network"))?;
        let source = claimed.source;
        let (factory, implementation) = (
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
        );
        if review.proxy != proxy
            || review.token != token
            || proxy != proxy_address(factory, implementation, source)
        {
            return Err(eyre!(
                "the withdrawal differs from this swap's proxy and bought token; review it again"
            ));
        }
        let (max_fee_per_gas, _) = reviewed_gas_fee(gas_fee)?;
        if U256::from(review.gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .is_none_or(|cost| cost > review.max_gas_cost)
        {
            return Err(eyre!(
                "the withdrawal can cost more in gas than reviewed; review it again"
            ));
        }

        let calls = withdrawal_calls(token, source, review.amount)?;
        // The batch about to be signed is read back: it pays only the Public account.
        if decode_withdrawal_calls(&calls, source)? != (token, review.amount) {
            return Err(eyre!(
                "the withdrawal batch differs from the reviewed withdrawal; nothing was signed"
            ));
        }
        let hooks = ExecuteHooks {
            calls,
            nonce: new_public_swap_batch_nonce()?,
            deadline: U256::from(valid_to_after(
                SystemTime::now(),
                profile.valid_to_window(),
            )?),
        };
        let chain_id = origin.chain_id;
        let signature = self
            .while_active(sign_public_swap_typed_data(
                signer,
                execute_hooks_typed_data(&hooks, chain_id, proxy)?,
                execute_hooks_digest(&hooks, chain_id, proxy),
                hash_fallback_confirmed,
            ))
            .await?;
        let calldata =
            execute_hooks_calldata(hooks, source, &signature, chain_id, factory, implementation)?;
        let mut transaction = TransactionRequest::default()
            .to(factory)
            .input(calldata.into());
        transaction.chain_id = Some(chain_id);
        transaction.from = Some(source);
        transaction.gas = Some(review.gas_limit);
        let outcome = self
            .submit_public_swap_transaction(
                operation,
                swap_use,
                origin,
                signer,
                PublicSwapTransactionKind::Withdrawal,
                PublicActionProgressStep::Send,
                transaction,
                gas_fee,
                client,
                progress,
            )
            .await?;
        let outcome = transaction_outcome(&outcome)?;
        // The withdrawal counts once its block is final. A read that fails here is repeated by
        // the swap's next observation, so it doesn't fail a withdrawal that was sent.
        if matches!(outcome, PublicSwapTransactionOutcome::Included { .. }) {
            let _ =
                Box::pin(self.advance_public_swap_order(operation, swap_use, origin, None, client))
                    .await;
        }
        Ok(outcome)
    }

    /// Cancel an open order early: the Public account invalidates it on chain. The hook batch
    /// is never revoked.
    pub async fn submit_public_swap_invalidation(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        orderbook: &CowOrderbookClient,
        max_gas_limit: u64,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_invalidation_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            orderbook,
            max_gas_limit,
            client,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_invalidation`] with the Public account's signer.
    ///
    /// The one transaction is `GPv2Settlement.invalidateOrder` from the account to the pinned
    /// settlement. Nothing is sent to the proxy or the factory: revoking the batch's nonce while
    /// the order can still fill would turn a fill into proceeds the proxy holds, so the batch
    /// stays valid until its deadline. The invalidation succeeds on an order that already
    /// filled, so its inclusion says nothing of the outcome: the order's trade is looked up
    /// afterwards, and the swap counts as cancelled only when the orderbook reports none.
    pub(crate) async fn submit_public_swap_invalidation_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        orderbook: &CowOrderbookClient,
        max_gas_limit: u64,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_signer(signer, &claimed)?;
        // An order past its `validTo` stays open until its end is read, and can't fill.
        if public_swap_order_state(&claimed.swap) != Some(PublicSwapOrderState::Open)
            || !claimed.swap.order_can_fill(unix_now()?)
        {
            return Err(eyre!("only an open order can be cancelled"));
        }
        let order = claimed
            .swap
            .order()
            .ok_or_else(|| eyre!("this swap has no signed order"))?;
        let settlement = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!("swaps from a Public account are unavailable on this network"))?
            .settlement();
        reviewed_gas_fee(gas_fee)?;
        let mut transaction = TransactionRequest::default()
            .to(settlement)
            .input(invalidate_order_calldata(&order.uid()).into());
        transaction.chain_id = Some(origin.chain_id);
        transaction.from = Some(claimed.source);
        transaction.gas = Some(max_gas_limit);
        let outcome = self
            .submit_public_swap_transaction(
                operation,
                swap_use,
                origin,
                signer,
                PublicSwapTransactionKind::Invalidation,
                PublicActionProgressStep::Send,
                transaction,
                gas_fee,
                client,
                progress,
            )
            .await?;
        let outcome = transaction_outcome(&outcome)?;
        // A trade decides, and only without one is the swap cancelled. A lookup that fails here
        // is repeated by the swap's next observation, so it doesn't fail an invalidation that
        // was sent.
        if matches!(outcome, PublicSwapTransactionOutcome::Included { .. }) {
            let _ = Box::pin(
                self.observe_public_swap_order(operation, swap_use, origin, orderbook, client),
            )
            .await;
        }
        Ok(outcome)
    }
}
