use super::*;

impl ExecutorOwner {
    /// Read how the Public account lets `spender` take the swap's Sell token on the origin
    /// chain and plan its transactions at the given fee. An order, which makes no `deposit`,
    /// pays no gas for an approval its token lets it sign.
    pub async fn plan_public_swap_gas(
        &self,
        origin: &EffectiveChainConfig,
        source: Address,
        sell_token: Address,
        spender: Address,
        amount: U256,
        deposit: bool,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        client: &OperationHttpClient,
    ) -> Result<PublicSwapGasPlan> {
        let plan = self
            .public_swap_approval_plan(
                origin, source, sell_token, spender, amount, !deposit, client,
            )
            .await?;
        public_swap_gas_plan(
            origin,
            plan.transactions().len(),
            deposit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        )
    }

    /// Send the approvals the swap needs from its Public account, each persisted before it is
    /// broadcast. A plan the review didn't price sends nothing and returns to review.
    pub async fn submit_public_swap_approvals(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapApprovalsOutcome> {
        Box::pin(self.submit_public_swap_approvals_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            client,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_approvals`] with the Public account's signer.
    ///
    /// The approval plan is read again and decides what is still needed, so running this again
    /// after a restart sends only the approvals that are missing. An order approved with a
    /// permit sends nothing while its token still lets it sign one. Transactions the saved
    /// approval's gas maximum doesn't cover, and a permit the order wasn't priced with, send
    /// nothing either and need another review.
    pub(crate) async fn submit_public_swap_approvals_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        mut gas_fee: PublicActionGasFeeSelection,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapApprovalsOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_source(signer, &claimed)?;
        let (approval, intent) = (claimed.swap.approval(), claimed.swap.intent());
        let sell_token = approval.sell_token;
        // The native asset is deposited as value and takes no approval.
        if sell_token == Address::ZERO {
            return Ok(PublicSwapApprovalsOutcome::Ready);
        }
        let amount = approval.bounds.sell_amount;
        // An order's Sell token is pulled by `CoW`'s vault relayer, a deposit's by the pool.
        let spender = if intent.order {
            origin
                .public_swap_profile()
                .ok_or_else(|| {
                    eyre!("swaps from a Public account are unavailable on this network")
                })?
                .vault_relayer()
        } else {
            claimed.spoke_pool
        };
        let plan = self
            .public_swap_approval_plan(
                origin,
                claimed.source,
                sell_token,
                spender,
                amount,
                intent.order,
                client,
            )
            .await?;
        if plan.signs_permit() {
            // The order signs the permit. One whose hook the approval didn't price is quoted
            // again.
            return Ok(if approval.bounds.pre_hook_gas_limit == 0 {
                PublicSwapApprovalsOutcome::ReviewRequired {
                    transactions: false,
                }
            } else {
                PublicSwapApprovalsOutcome::Ready
            });
        }
        let mut remaining = plan.transactions();
        if !remaining.is_empty() {
            // Transactions the review didn't price are reviewed before any of them is sent.
            let (max_fee_per_gas, max_priority_fee_per_gas) = reviewed_gas_fee(gas_fee)?;
            let needed = public_swap_gas_plan(
                origin,
                remaining.len(),
                !intent.order,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )?;
            if needed.max_gas_cost > approval.max_gas_cost {
                return Ok(PublicSwapApprovalsOutcome::ReviewRequired { transactions: true });
            }
        }
        let mut nonce = None;
        while let Some((value, later)) = remaining.split_first() {
            let (max_fee_per_gas, max_priority_fee_per_gas) = reviewed_gas_fee(gas_fee)?;
            let plan = public_swap_gas_plan(
                origin,
                remaining.len(),
                !intent.order,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )?;
            require_approved_gas_cost(&plan, approval.max_gas_cost)?;
            let gas_limit = plan
                .approval_gas_limits
                .first()
                .copied()
                .ok_or_else(|| eyre!("the swap's gas plan has no approval"))?;
            let mut transaction = TransactionRequest::default()
                .to(sell_token)
                .input(Bytes::from(build_approve_calldata(spender, *value)).into());
            transaction.chain_id = Some(origin.chain_id);
            transaction.from = Some(claimed.source);
            transaction.nonce = nonce;
            transaction.gas = Some(gas_limit);
            let kind = if value.is_zero() {
                PublicSwapTransactionKind::ApprovalReset
            } else {
                PublicSwapTransactionKind::Approval
            };
            let outcome = self
                .submit_public_swap_transaction(
                    operation,
                    swap_use,
                    origin,
                    signer,
                    kind,
                    PublicActionProgressStep::Approve,
                    transaction,
                    gas_fee,
                    client,
                    progress,
                )
                .await?;
            if !outcome.receipt.status {
                return Err(eyre!(
                    "the Public account's approval reverted ({})",
                    outcome.receipt.tx_hash
                ));
            }
            nonce = Some(outcome.next_nonce);
            gas_fee = outcome.gas_fee;
            remaining = later;
        }
        Ok(PublicSwapApprovalsOutcome::Ready)
    }

    /// Send the swap's deposit from its Public account, for a signed delivery.
    pub async fn submit_public_swap_deposit(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_deposit_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            delivery,
            terms,
            client,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_deposit`] with the Public account's signer.
    ///
    /// The path and the signed terms are in the record before the deposit is signed, and the
    /// deposit, with its signed bytes, is in it before it is broadcast. A deposit an RPC
    /// accepted is followed to its receipt, whose provisional inclusion is recorded. One that
    /// no RPC accepted returns [`PublicSwapTransactionOutcome::HandedOff`] without a receipt,
    /// and tracking follows it from the record. A deposit that was ruled out takes another.
    pub(crate) async fn submit_public_swap_deposit_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_source(signer, &claimed)?;
        let (approval, intent) = (claimed.swap.approval(), claimed.swap.intent());
        if intent.order {
            return Err(eyre!(
                "this swap places an order; its Public account sends no deposit"
            ));
        }
        if claimed.swap.live_deposits().next().is_some() {
            return Err(eyre!(
                "this swap's deposit was already handed off; check its progress"
            ));
        }
        if claimed.destination != Some(delivery.destination_executor) {
            return Err(eyre!(
                "the destination stealth account changed; review the swap again"
            ));
        }
        let destination_minimum = approval
            .bounds
            .destination_minimum
            .ok_or_else(|| eyre!("the swap's approval has no destination minimum"))?;
        if terms.spoke_pool != claimed.spoke_pool
            || terms.recipient != Some(delivery.handler)
            || terms.input_token != intent.bridged_token
            || terms.output_token != claimed.destination_token
            || terms.output_amount < destination_minimum
            || terms.input_amount != approval.bounds.sell_amount
        {
            return Err(eyre!(
                "the deposit's terms differ from the approved swap; review the swap again"
            ));
        }
        let (max_fee_per_gas, max_priority_fee_per_gas) = reviewed_gas_fee(gas_fee)?;
        let plan =
            public_swap_gas_plan(origin, 0, true, max_fee_per_gas, max_priority_fee_per_gas)?;
        require_approved_gas_cost(&plan, approval.max_gas_cost)?;
        let gas_limit = plan
            .deposit_gas_limit
            .ok_or_else(|| eyre!("the swap's gas plan has no deposit"))?;

        let deposit = SpokePool::depositV3Call {
            depositor: claimed.source,
            recipient: delivery.handler,
            inputToken: terms.input_token,
            outputToken: terms.output_token,
            inputAmount: terms.input_amount,
            outputAmount: terms.output_amount,
            destinationChainId: U256::from(self.chain.chain_id),
            exclusiveRelayer: terms.exclusive_relayer,
            quoteTimestamp: terms.quote_timestamp,
            fillDeadline: terms.fill_deadline,
            exclusivityParameter: terms.exclusivity_parameter,
            message: Bytes::new(),
        };
        let calldata = private_deposit_calldata(
            deposit,
            delivery.handler,
            delivery.destination_executor,
            delivery.shield_multicall.clone(),
            delivery.fallback,
        )?;
        // The fill is matched on the message Across quoted, so the deposit must carry it.
        let message = SpokePool::depositV3Call::abi_decode(&calldata)?.message;
        if terms.message_hash != Some(keccak256(&message)) {
            return Err(eyre!(
                "the deposit's message differs from the one Across quoted; review the swap again"
            ));
        }
        // The native asset is deposited as its wrapped token, paid as the transaction's value.
        let value = if approval.sell_token == Address::ZERO {
            terms.input_amount
        } else {
            U256::ZERO
        };
        let mut transaction = TransactionRequest::default()
            .to(claimed.spoke_pool)
            .value(value)
            .input(calldata.into());
        transaction.chain_id = Some(origin.chain_id);
        transaction.from = Some(claimed.source);
        transaction.gas = Some(gas_limit);

        self.ensure_active()?;
        self.store
            .record_public_swap_path(operation, swap_use, PublicSwapPath::Deposit, *terms)?;
        self.notify_change();
        let sent = Box::pin(self.send_public_swap_transaction(
            operation,
            swap_use,
            origin,
            signer,
            PublicSwapTransactionKind::Deposit,
            PublicActionProgressStep::Send,
            transaction,
            gas_fee,
            PublicActionStepEnd::HandOff,
            client,
            progress,
        ))
        .await?;
        match sent {
            PublicSwapSent::HandedOff(transaction_hash) => {
                Ok(PublicSwapTransactionOutcome::HandedOff { transaction_hash })
            }
            PublicSwapSent::Included(outcome) => transaction_outcome(&outcome),
        }
    }
}
