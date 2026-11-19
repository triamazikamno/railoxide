use super::*;

impl ExecutorOwner {
    /// Read the deposit's hand-off from finalized whole-block receipts on the origin chain and
    /// record it. `None` while it isn't final or found.
    ///
    /// A deposit with a recorded inclusion is read in that block once it is final. Otherwise
    /// at most eight finalized blocks are inspected per poll from the durable hand-off
    /// boundary, matching the hash locally and saving progress for restart. A deposit those
    /// blocks don't hold, with no inclusion or one its final block no longer holds, is ruled
    /// out once the Public account's nonce at the last of them, still canonical, passed its
    /// own. A stopped swap's deposit is read too, and a ruled-out deposit isn't. The reads go
    /// over `client`.
    pub async fn observe_public_swap_handoff(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        client: &OperationHttpClient,
    ) -> Result<Option<SwapBridgeHandoff>> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let swap = &claimed.swap;
        if let Some(handoff) = swap.observations().bridge_handoff {
            return Ok(Some(handoff));
        }
        let (Some(PublicSwapPath::Deposit), Some(terms)) = (swap.path(), swap.bridge()) else {
            return Ok(None);
        };
        let deposits: Vec<_> = swap.live_deposits().collect();
        if deposits.is_empty() {
            return Ok(None);
        }
        let expected = ExpectedHandoff {
            spoke_pool: claimed.spoke_pool,
            source: claimed.source,
            destination_chain: self.chain.chain_id,
            terms,
        };
        let endpoints = ObservationEndpoints::new(
            origin,
            &self.http,
            crate::RpcTrafficClass::Sensitive,
            client,
        )?;
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "public_swap_handoff",
                self.while_active(Box::pin(read_public_swap_handoff(
                    &endpoint.provider,
                    origin.finality_depth,
                    &deposits,
                    &expected,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(read) => {
                    endpoints.succeeded(&endpoint);
                    for update in read.updates {
                        if let Some(inclusion) = update.inclusion {
                            self.store.record_public_swap_inclusion(
                                operation,
                                swap_use,
                                update.hash,
                                inclusion,
                            )?;
                        }
                        if let Some(next_block) = update.next_block {
                            self.store.record_public_swap_deposit_scan(
                                operation,
                                swap_use,
                                update.hash,
                                next_block,
                            )?;
                        }
                        if let Some(evidence) = update.ruled_out {
                            self.store.record_public_swap_deposit_ruled_out(
                                operation,
                                swap_use,
                                update.hash,
                                evidence,
                            )?;
                        }
                    }
                    self.notify_change();
                    return match read.handoff {
                        Some(found) => self
                            .record_public_swap_handoff(operation, swap_use, &found)
                            .await
                            .map(Some),
                        None => Ok(None),
                    };
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Deposit verification on the network the swap pays on is unavailable. The swap will be checked again."
        ))
    }

    /// Persist a verified hand-off: the inclusion of its transaction, when the record holds
    /// another or none, and the deposit with its amounts beside what was already observed.
    async fn record_public_swap_handoff(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        found: &ObservedHandoff,
    ) -> Result<SwapBridgeHandoff> {
        let _guard = self.lock_activity().await;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let (_, swap) = record.public_swap_use(swap_use).ok_or_else(|| {
            eyre!("this stealth account isn't claimed by a swap paid from a Public account")
        })?;
        let observation = SwapObservation {
            block: found.block,
            transaction_hash: Some(found.transaction_hash),
        };
        let inclusion = PublicSwapInclusion {
            observation,
            succeeded: true,
            finalized: true,
        };
        if !swap.transactions().iter().any(|transaction| {
            transaction.hash == found.transaction_hash && transaction.inclusion == Some(inclusion)
        }) {
            self.store.record_public_swap_inclusion(
                operation,
                swap_use,
                found.transaction_hash,
                inclusion,
            )?;
        }
        let handoff = SwapBridgeHandoff {
            observation,
            deposit_id: Some(found.deposit_id),
        };
        self.store.record_public_swap_observations(
            operation,
            swap_use,
            PublicSwapObservations {
                bridge_handoff: Some(handoff),
                deposited: Some(found.deposited),
                ..swap.observations()
            },
        )?;
        self.notify_change();
        Ok(handoff)
    }
}
