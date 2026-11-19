use super::*;

impl ExecutorOwner {
    /// The swap of the use `swap_use`, checked against the chain `origin` it pays on.
    pub(in crate::desktop::executors::swap) fn claimed_public_swap(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
    ) -> Result<ClaimedPublicSwap> {
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            swap,
            ..
        }) = record.swap_use(swap_use).map(SwapUseRecord::role)
        else {
            return Err(eyre!(
                "this stealth account isn't claimed by a swap paid from a Public account"
            ));
        };
        if origin.chain_id != *origin_chain {
            return Err(eyre!(
                "the network the swap pays on differs from the one it was claimed for"
            ));
        }
        let spoke_pool = origin
            .bridge_origin_profile()
            .ok_or_else(|| eyre!("this network has no pinned Across SpokePool"))?
            .spoke_pool();
        Ok(ClaimedPublicSwap {
            source: *source,
            destination_token: *destination_token,
            destination: record.address(),
            swap: (**swap).clone(),
            live: is_live_swap_use(&record, swap_use),
            spoke_pool,
        })
    }

    /// Admit the Public source and resolve its signer without signing or broadcasting.
    /// Call before destination setup so both signing permissions are obtained before the
    /// swap's first side effect. Retain the authorized source for its approvals, deposit or order.
    pub async fn authorize_public_swap_source(
        &self,
        origin: &EffectiveChainConfig,
        source: PublicSwapSource<'_>,
        client: &OperationHttpClient,
    ) -> Result<AuthorizedPublicSwapSource> {
        self.ensure_active()?;
        let signer = Box::pin(admitted_public_signer_authorized(
            &self.vault,
            &self.view,
            source.authorization,
            source.public_account_uuid,
            source.trezor_app_passphrase,
            source.trezor_pin_matrix_provider,
            None,
            origin.chain_id,
            client,
        ))
        .await?;
        Ok(AuthorizedPublicSwapSource { signer })
    }

    /// The Public account's allowance of `token` to `spender` on the origin chain.
    pub(in crate::desktop::executors::swap) async fn public_swap_allowance(
        &self,
        origin: &EffectiveChainConfig,
        source: Address,
        token: Address,
        spender: Address,
        client: &OperationHttpClient,
    ) -> Result<U256> {
        self.while_active(query_erc20_allowance(
            &origin
                .rpc_route
                .for_class(crate::RpcTrafficClass::Sensitive)?
                .isolated(client.clone()),
            &self.http,
            PublicAssetId::Erc20(token),
            source,
            spender,
        ))
        .await
        .wrap_err("read the Public account's allowance for the swap")
    }

    /// Send one transaction of the swap from its Public account. It is recorded with the
    /// origin chain's head before it is broadcast, and its inclusion once the step observed
    /// it, whether it succeeded or reverted. A stopped swap sends no approval and no deposit.
    /// Its withdrawal and its order's invalidation are still sent.
    pub(in crate::desktop::executors::swap) async fn submit_public_swap_transaction(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        kind: PublicSwapTransactionKind,
        step: PublicActionProgressStep,
        transaction: TransactionRequest,
        gas_fee: PublicActionGasFeeSelection,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapStepOutcome> {
        match Box::pin(self.send_public_swap_transaction(
            operation,
            swap_use,
            origin,
            signer,
            kind,
            step,
            transaction,
            gas_fee,
            PublicActionStepEnd::Receipt,
            client,
            progress,
        ))
        .await?
        {
            PublicSwapSent::Included(outcome) => Ok(outcome),
            PublicSwapSent::HandedOff(_) => Err(eyre!(
                "the Public account's transaction was sent without its receipt"
            )),
        }
    }

    /// [`Self::submit_public_swap_transaction`], ending where `end` says: with
    /// [`PublicActionStepEnd::HandOff`], a transaction no RPC accepted is
    /// [`PublicSwapSent::HandedOff`] without a receipt. A deposit is
    /// recorded with its signed bytes and a scan boundary the origin chain's finality depth
    /// below the head: a deposit signed after that head can't be included below it.
    pub(super) async fn send_public_swap_transaction(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        kind: PublicSwapTransactionKind,
        step: PublicActionProgressStep,
        transaction: TransactionRequest,
        gas_fee: PublicActionGasFeeSelection,
        end: PublicActionStepEnd,
        client: &OperationHttpClient,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapSent> {
        // The swap's client covers the step's head read, submission and inclusion read.
        let submitted_from_block = self
            .while_active(origin_head(origin, &self.http, client))
            .await?;
        let ends_swap = matches!(
            kind,
            PublicSwapTransactionKind::Withdrawal | PublicSwapTransactionKind::Invalidation
        );
        let deposit = kind == PublicSwapTransactionKind::Deposit;
        let mut handed_off = None;
        let mut handoff = |hash, transaction: &TransactionRequest, signed: &[u8]| {
            if !ends_swap {
                self.require_live_swap_use(operation, Some(swap_use))?;
            }
            self.store.record_public_swap_transaction(
                operation,
                swap_use,
                PublicSwapTransaction {
                    kind,
                    transaction: transaction.clone(),
                    hash,
                    inclusion: None,
                    submitted_from_block: Some(submitted_from_block),
                    deposit_scan_from_block: deposit
                        .then(|| submitted_from_block.saturating_sub(origin.finality_depth)),
                    signed: deposit
                        .then(|| PublicSwapSignedTransaction::new(Bytes::copy_from_slice(signed))),
                    ruled_out: None,
                },
            )?;
            handed_off = Some(hash);
            self.notify_change();
            Ok(())
        };
        let outcome = self
            .while_active(Box::pin(submit_public_swap_step(
                step,
                transaction,
                signer,
                origin,
                gas_fee,
                &self.http,
                client,
                &mut handoff,
                end,
                progress,
            )))
            .await?;
        let Some(outcome) = outcome else {
            return handed_off
                .map(PublicSwapSent::HandedOff)
                .ok_or_else(|| eyre!("the Public account's transaction wasn't handed off"));
        };
        let hash: B256 = outcome.receipt.tx_hash.parse()?;
        // The step reports the block's number. Its hash is read by that number, and the
        // hand-off's read checks the transaction against the final block again.
        let block = origin_block(origin, &self.http, client, outcome.receipt.block_number).await?;
        self.store.record_public_swap_inclusion(
            operation,
            swap_use,
            hash,
            PublicSwapInclusion {
                observation: SwapObservation {
                    block,
                    transaction_hash: Some(hash),
                },
                succeeded: outcome.receipt.status,
                finalized: false,
            },
        )?;
        self.notify_change();
        Ok(PublicSwapSent::Included(outcome))
    }

    /// Resend, unchanged, the signed bytes of the swap's deposit that is neither included nor
    /// ruled out, as the current record holds them. Nothing is signed and nothing is sent for
    /// a swap that was stopped or no longer claims its account. An RPC that already knows the
    /// deposit accepts it again. A send no RPC accepted says nothing of the deposit, which
    /// tracking keeps reading.
    pub(in crate::desktop::executors::swap) async fn resend_public_swap_deposit(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        client: &OperationHttpClient,
    ) -> Result<()> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        if !claimed.live || claimed.swap.observations().bridge_handoff.is_some() {
            return Ok(());
        }
        let unresolved: Vec<_> = claimed
            .swap
            .live_deposits()
            .filter(|deposit| deposit.inclusion.is_none())
            .filter_map(|deposit| Some((deposit.hash, deposit.signed.clone()?)))
            .collect();
        if unresolved.is_empty() {
            return Ok(());
        }
        // Stored signed deposits are resent only where submission is allowed, over the swap's
        // client.
        let pool = query_rpc_pool_for_route(
            &origin
                .rpc_route
                .for_class(crate::RpcTrafficClass::Submission)?
                .isolated(client.clone()),
            &self.http,
        );
        for (hash, signed) in unresolved {
            self.while_active(
                self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
                    &pool,
                    self.http.network_mode(),
                    signed.raw().to_vec(),
                    hash,
                    false,
                ),
            )
            .await?;
        }
        Ok(())
    }
}
