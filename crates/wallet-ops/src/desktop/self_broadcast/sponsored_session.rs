use super::*;

const SPONSORED_HEAD_POLL_MIN_INTERVAL: Duration = Duration::from_millis(500);
const SPONSORED_HEAD_POLL_MAX_INTERVAL: Duration = Duration::from_secs(3);
pub(super) const SPONSORED_PENDING_LOCK_RENEW_INTERVAL: Duration = Duration::from_mins(5);
const SPONSORED_PENDING_MARK_TIMEOUT: Duration = Duration::from_secs(5);
const SPONSORED_CANCEL_GRACE_MIN: Duration = Duration::from_secs(15);
const SPONSORED_CANCEL_GRACE_MAX: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SponsoredSelfBroadcastCommand {
    #[default]
    Running,
    Cancel,
    Shutdown,
}

pub type SponsoredSelfBroadcastCommandSender = watch::Sender<SponsoredSelfBroadcastCommand>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredSelfBroadcastStopReason {
    Cancelled,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SponsoredSelfBroadcastSessionOutcome {
    CanonicalReceipt(TxReceiptOutput),
    Stopped {
        reason: SponsoredSelfBroadcastStopReason,
        bundle_was_accepted: bool,
    },
}

pub struct SponsoredSelfBroadcastSessionRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub session: Arc<WalletSession>,
    pub signed_raw_transaction: Vec<u8>,
    pub pending_spent_inputs: Vec<Utxo>,
    pub command_rx: watch::Receiver<SponsoredSelfBroadcastCommand>,
}

#[must_use]
pub fn sponsored_self_broadcast_head_poll_interval(block_time: Duration) -> Duration {
    (block_time / 4).clamp(
        SPONSORED_HEAD_POLL_MIN_INTERVAL,
        SPONSORED_HEAD_POLL_MAX_INTERVAL,
    )
}

#[must_use]
pub(crate) fn sponsored_self_broadcast_cancel_grace(block_time: Duration) -> Duration {
    block_time
        .saturating_mul(2)
        .clamp(SPONSORED_CANCEL_GRACE_MIN, SPONSORED_CANCEL_GRACE_MAX)
}

/// Runs one signed sponsored transaction until a canonical receipt or local stop.
///
/// The signed payload is owned only by this call. It is reused for every relay
/// attempt and is neither persisted nor submitted through normal chain RPCs.
///
/// It observes through `query_rpc_pool`, the pool over the operation's isolated Sensitive route
/// `sensitive_route` that prepared and submitted it. When `activity` is present, its hash is
/// recorded there before the first relay handoff.
async fn run_sponsored_self_broadcast_session(
    request: SponsoredSelfBroadcastSessionRequest,
    activity: Option<&ActivitySubmission>,
    http: &HttpContext,
    client: &OperationHttpClient,
    sensitive_route: ClassRoute,
    query_rpc_pool: Arc<QueryRpcPool>,
) -> Result<SponsoredSelfBroadcastSessionOutcome> {
    if railgun_ui::is_testnet_chain(request.chain_id) {
        return Err(eyre!(
            "Sponsored self-broadcast is unavailable on testnets."
        ));
    }
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    if request.effective_chain.chain_id != request.chain_id {
        return Err(eyre!(
            "effective chain config is for chain {}, not {}",
            request.effective_chain.chain_id,
            request.chain_id
        ));
    }
    let block_time = request
        .effective_chain
        .block_time
        .ok_or_else(|| eyre!("sponsored self-broadcast requires a block cadence"))?;
    if block_time.is_zero() {
        return Err(eyre!(
            "sponsored self-broadcast block time must be non-zero"
        ));
    }
    if request
        .effective_chain
        .require_railgun()?
        .sponsored_bundle_relays
        .is_empty()
    {
        return Err(eyre!(
            "sponsored self-broadcast requires at least one relay"
        ));
    }

    let tx_hash = keccak256(&request.signed_raw_transaction);
    let signed_raw_transaction = Zeroizing::new(hex::encode_prefixed(
        Zeroizing::new(request.signed_raw_transaction).as_slice(),
    ));
    let backend = LiveSponsoredSelfBroadcastBackend {
        transaction_tracking: request.transaction_tracking.as_ref(),
        http,
        client,
        relays: &request
            .effective_chain
            .require_railgun()?
            .sponsored_bundle_relays,
        query_rpc_pool,
        sensitive_route,
        finality_depth: request.effective_chain.finality_depth,
        tx_hash,
        observer: tokio::sync::Mutex::new(None),
        session: &request.session,
        pending_spent_inputs: &request.pending_spent_inputs,
        activity,
    };

    run_sponsored_self_broadcast_session_with_backend(
        &backend,
        block_time,
        tx_hash,
        signed_raw_transaction.as_str(),
        request.command_rx,
    )
    .await
}

pub(super) fn resolve_sponsored_pending_spent_inputs(
    available: Vec<Utxo>,
    selected: &[SelectedInputIdentity],
) -> Result<Vec<Utxo>> {
    let mut by_identity = available
        .into_iter()
        .map(|utxo| (SelectedInputIdentity::from_utxo(&utxo), utxo))
        .collect::<BTreeMap<_, _>>();
    let mut resolved = Vec::with_capacity(selected.len());
    for identity in selected {
        let utxo = by_identity.remove(identity).ok_or_else(|| {
            eyre!(
                "sponsored selected input at tree {} position {} is no longer spendable",
                identity.tree,
                identity.position
            )
        })?;
        resolved.push(utxo);
    }
    Ok(resolved)
}

pub(in crate::desktop) async fn submit_prepared_sponsored_self_broadcast(
    mut request: DesktopPreparedSponsoredSelfBroadcastRequest,
    activity: Option<&ActivitySubmission>,
    http: &HttpContext,
    client: &OperationHttpClient,
    sensitive_route: ClassRoute,
    query_rpc_pool: Arc<QueryRpcPool>,
) -> Result<SponsoredSelfBroadcastSessionOutcome> {
    if request.prepared.chain_id != request.chain_id {
        return Err(eyre!(
            "prepared sponsored call is for chain {}, not {}",
            request.prepared.chain_id,
            request.chain_id
        ));
    }
    if request.prepared.authorization.delivery != PrivateDeliveryMode::SelfBroadcast {
        return Err(SponsorshipError::DeliveryUnsupported.into());
    }

    let signer_address = self_broadcast_gas_payer(
        &request.vault_store,
        &request.view_session,
        &request.public_account_uuid,
    )?;
    if signer_address != request.prepared.authorization.signer {
        return Err(eyre!(
            "selected sponsored signer does not match the authorized signer"
        ));
    }
    let payer = request
        .effective_chain
        .require_railgun()?
        .coinbase_payer
        .ok_or(SponsorshipError::MissingCoinbasePayer)?;
    if payer != request.prepared.authorization.coinbase_payer {
        return Err(eyre!(
            "effective coinbase payer does not match the authorized payer"
        ));
    }

    let provider_handle =
        sponsored_code_preflight_from_rpc_pool(&query_rpc_pool, payer, signer_address).await?;
    let nonce = provider_handle
        .provider
        .get_transaction_count(signer_address)
        .await
        .map_err(|_| eyre!("sponsored self-broadcast nonce query failed"))?;
    let pending_spent_inputs = resolve_sponsored_pending_spent_inputs(
        request.session.unspent_utxos(),
        &request.prepared.selected_inputs,
    )?;
    if pending_spent_inputs.len() != request.prepared.input_count {
        return Err(eyre!(
            "prepared sponsored input count does not match selected input identities"
        ));
    }

    if let Some(reason) = sponsored_pending_stop_reason(&request.command_rx) {
        return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason,
            bundle_was_accepted: false,
        });
    }

    let vault_password = request.vault_password.take();
    let signer =
        if let Some(authorization @ DesktopPrivateSpendAuthorization::HardwareExecutor(hardware)) =
            request.gas_payer_authorization.as_ref()
            && hardware.is_gas_payment_for(&request.public_account_uuid)
        {
            let account = request
                .vault_store
                .list_public_accounts_for_session(&request.view_session, true)?
                .into_iter()
                .find(|account| {
                    account.public_account_uuid == request.public_account_uuid
                        && account.is_available_on_chain(request.chain_id)
                })
                .ok_or_else(|| eyre!("reviewed gas payer is unavailable"))?;
            let owner = request
                .session
                .executor_owner()
                .ok_or_else(|| eyre!("Open the owning wallet and chain before signing"))?;
            let (signer, guard) = Box::pin(owner.admit_authorized_gas_signer(
                &request.view_session,
                &account,
                authorization,
                client,
            ))
            .await?;
            VaultedPublicSigner::Executor(signer, guard)
        } else {
            admitted_public_signer(
                &request.vault_store,
                &request.view_session,
                vault_password.as_ref().map(|password| password.as_str()),
                &request.public_account_uuid,
                request.protected_software_seed_session.as_deref(),
                None,
                request.trezor_pin_matrix_provider,
                request.session.executor_owner().as_ref(),
                request.chain_id,
                client,
            )
            .await?
        };
    signer
        .while_active(async {
            drop(vault_password);
            if signer.address() != signer_address {
                return Err(eyre!(
                    "selected public account signer address does not match account metadata"
                ));
            }
            let data: Bytes = request
                .prepared
                .data
                .parse()
                .wrap_err("decode prepared sponsored calldata")?;
            let authorization = request.prepared.authorization;
            let tx_req = self_broadcast_transaction_request(
                request.chain_id,
                signer_address,
                TransactionRequest::default()
                    .to(request.prepared.to)
                    .input(data.into()),
                authorization.max_fee_per_gas,
                authorization.max_priority_fee_per_gas,
                nonce,
            )?
            .with_gas_limit(authorization.transaction_gas_limit);

            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::SigningSelfBroadcast,
            );
            if let Some(reason) = sponsored_pending_stop_reason(&request.command_rx) {
                return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                    reason,
                    bundle_was_accepted: false,
                });
            }
            let signed_raw_transaction = {
                let signing = signer.sign_transaction_request(tx_req, "sponsored self-broadcast");
                tokio::pin!(signing);
                tokio::select! {
                    result = &mut signing => result?,
                    changed = request.command_rx.changed() => {
                        let reason = if changed.is_err() {
                            SponsoredSelfBroadcastStopReason::Shutdown
                        } else {
                            sponsored_stop_reason(*request.command_rx.borrow_and_update())
                                .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                        };
                        return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                            reason,
                            bundle_was_accepted: false,
                        });
                    }
                }
            };
            emit_refreshed_self_broadcast_hardware_session(request.event_tx.as_ref(), &signer);
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
            );
            run_sponsored_self_broadcast_session(
                SponsoredSelfBroadcastSessionRequest {
                    transaction_tracking: request.transaction_tracking,
                    chain_id: request.chain_id,
                    effective_chain: request.effective_chain,
                    session: request.session,
                    signed_raw_transaction,
                    pending_spent_inputs,
                    command_rx: request.command_rx,
                },
                activity,
                http,
                client,
                sensitive_route,
                query_rpc_pool,
            )
            .await
        })
        .await
}

fn sponsored_pending_stop_reason(
    command_rx: &watch::Receiver<SponsoredSelfBroadcastCommand>,
) -> Option<SponsoredSelfBroadcastStopReason> {
    if command_rx.has_changed().is_err() {
        sponsored_stop_reason(*command_rx.borrow())
            .or(Some(SponsoredSelfBroadcastStopReason::Shutdown))
    } else {
        sponsored_stop_reason(*command_rx.borrow())
    }
}

#[async_trait]
pub(super) trait SponsoredSelfBroadcastBackend {
    async fn establish_observation(&self) -> Result<()> {
        Ok(())
    }
    async fn submit_bundle(&self, signed_raw_transaction: &str, tx_hash: FixedBytes<32>) -> bool;
    async fn canonical_state(&self, tx_hash: FixedBytes<32>) -> SponsoredCanonicalState;
    async fn mark_inputs_pending_spent(&self, tx_hash: FixedBytes<32>) -> Result<()>;
}

#[derive(Default)]
pub(super) struct SponsoredCanonicalState {
    pub(super) receipt: Option<TxReceiptOutput>,
    pub(super) head: Option<(usize, u64)>,
    pub(super) confirmation_error: Option<&'static str>,
}

struct LiveSponsoredSelfBroadcastBackend<'a> {
    transaction_tracking: Option<&'a crate::PublicTransactionTrackingContext>,
    http: &'a HttpContext,
    client: &'a OperationHttpClient,
    relays: &'a [SensitiveUrl],
    query_rpc_pool: Arc<QueryRpcPool>,
    /// The route `query_rpc_pool` was built from.
    sensitive_route: ClassRoute,
    finality_depth: u64,
    tx_hash: FixedBytes<32>,
    observer: tokio::sync::Mutex<Option<PublicTransactionObservationGuard>>,
    session: &'a WalletSession,
    pending_spent_inputs: &'a [Utxo],
    activity: Option<&'a ActivitySubmission>,
}

#[async_trait]
impl SponsoredSelfBroadcastBackend for LiveSponsoredSelfBroadcastBackend<'_> {
    async fn establish_observation(&self) -> Result<()> {
        let mut observer = self.observer.lock().await;
        if observer.is_none() {
            let created = BlockObserver::establish(
                Arc::clone(&self.query_rpc_pool),
                self.finality_depth,
                self.http.rpc_broker(),
                &self.sensitive_route,
            )
            .await?;
            let mut created =
                PublicTransactionObservationGuard::new(created, self.transaction_tracking)?;
            created.register(self.tx_hash, 0);
            *observer = Some(created);
        }
        Ok(())
    }

    async fn submit_bundle(&self, signed_raw_transaction: &str, tx_hash: FixedBytes<32>) -> bool {
        if let Some(context) = self.transaction_tracking
            && context.ensure_open().is_err()
        {
            return false;
        }
        if let Some(activity) = self.activity {
            activity.add_attempt(tx_hash);
        }
        match submit_sponsored_bundle_with_acceptance(
            self.client,
            self.relays,
            signed_raw_transaction,
            || async {
                if tokio::time::timeout(
                    SPONSORED_PENDING_MARK_TIMEOUT,
                    self.session
                        .mark_pending_spent_utxos(self.pending_spent_inputs, Some(tx_hash)),
                )
                .await
                .is_err()
                {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "timed out marking sponsored inputs pending-spent after relay acceptance"
                    );
                }
            },
        )
        .await
        {
            Ok(fanout) => {
                for failure in &fanout.failures {
                    tracing::warn!(
                        target: LOG_TARGET,
                        relay = %failure.relay,
                        failure = ?failure.kind,
                        "sponsored self-broadcast relay rejected bundle"
                    );
                }
                tracing::info!(
                    target: LOG_TARGET,
                    accepted_relays = fanout.accepted_relays.len(),
                    failed_relays = fanout.failures.len(),
                    "sponsored self-broadcast bundle submitted"
                );
                true
            }
            Err(error) => {
                for failure in &error.failures {
                    tracing::warn!(
                        target: LOG_TARGET,
                        relay = %failure.relay,
                        failure = ?failure.kind,
                        "sponsored self-broadcast relay rejected bundle"
                    );
                }
                tracing::warn!(
                    target: LOG_TARGET,
                    failed_relays = error.failures.len(),
                    "sponsored self-broadcast bundle was not accepted"
                );
                false
            }
        }
    }

    async fn canonical_state(&self, _tx_hash: FixedBytes<32>) -> SponsoredCanonicalState {
        let mut observer = self.observer.lock().await;
        let Some(observer) = observer.as_mut() else {
            return SponsoredCanonicalState {
                confirmation_error: Some("observer was not established"),
                ..SponsoredCanonicalState::default()
            };
        };
        match observer.poll().await {
            Ok(observation) => SponsoredCanonicalState {
                receipt: observation.receipt.map(|(_, receipt)| receipt),
                head: observation.head,
                ..SponsoredCanonicalState::default()
            },
            Err(_) => SponsoredCanonicalState {
                confirmation_error: Some("block-scoped confirmation is unavailable"),
                ..SponsoredCanonicalState::default()
            },
        }
    }

    async fn mark_inputs_pending_spent(&self, tx_hash: FixedBytes<32>) -> Result<()> {
        self.session
            .renew_pending_spent_utxos(self.pending_spent_inputs, tx_hash)
            .await
    }
}

async fn submit_sponsored_bundle_latching_stop(
    backend: &impl SponsoredSelfBroadcastBackend,
    signed_raw_transaction: &str,
    tx_hash: FixedBytes<32>,
    command_rx: &mut watch::Receiver<SponsoredSelfBroadcastCommand>,
    command_open: &mut bool,
    stop_reason: &mut Option<SponsoredSelfBroadcastStopReason>,
) -> bool {
    let submission = backend.submit_bundle(signed_raw_transaction, tx_hash);
    tokio::pin!(submission);
    loop {
        tokio::select! {
            accepted = &mut submission => return accepted,
            changed = command_rx.changed(), if *command_open && *stop_reason != Some(SponsoredSelfBroadcastStopReason::Shutdown) => {
                *command_open = changed.is_ok();
                let observed = if changed.is_err() {
                    sponsored_stop_reason(*command_rx.borrow())
                        .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                } else {
                    sponsored_stop_reason(*command_rx.borrow_and_update())
                        .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                };
                *stop_reason = merge_sponsored_stop_reason(*stop_reason, Some(observed));
            }
        }
    }
}

async fn mark_sponsored_inputs_with_command(
    backend: &impl SponsoredSelfBroadcastBackend,
    tx_hash: FixedBytes<32>,
    command_rx: &mut watch::Receiver<SponsoredSelfBroadcastCommand>,
    command_open: &mut bool,
    stop_reason: &mut Option<SponsoredSelfBroadcastStopReason>,
    cancel_deadline: Option<tokio::time::Instant>,
) -> Result<bool> {
    let mark = tokio::time::timeout(
        SPONSORED_PENDING_MARK_TIMEOUT,
        backend.mark_inputs_pending_spent(tx_hash),
    );
    tokio::pin!(mark);
    tokio::select! {
        result = &mut mark => {
            result.map_err(|_| eyre!("timed out marking sponsored inputs pending-spent"))??;
            Ok(true)
        }
        changed = command_rx.changed(), if *command_open => {
            *command_open = changed.is_ok();
            let observed = if changed.is_err() {
                sponsored_stop_reason(*command_rx.borrow())
                    .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
            } else {
                sponsored_stop_reason(*command_rx.borrow_and_update())
                    .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
            };
            *stop_reason = merge_sponsored_stop_reason(*stop_reason, Some(observed));
            Ok(false)
        }
        () = wait_for_sponsored_cancel_deadline(cancel_deadline) => Ok(false),
    }
}

pub(super) async fn run_sponsored_self_broadcast_session_with_backend(
    backend: &(impl SponsoredSelfBroadcastBackend + Sync),
    block_time: Duration,
    tx_hash: FixedBytes<32>,
    signed_raw_transaction: &str,
    mut command_rx: watch::Receiver<SponsoredSelfBroadcastCommand>,
) -> Result<SponsoredSelfBroadcastSessionOutcome> {
    let poll_interval = sponsored_self_broadcast_head_poll_interval(block_time);
    let cancel_grace = sponsored_self_broadcast_cancel_grace(block_time);
    let mut stop_reason = sponsored_pending_stop_reason(&command_rx);
    let mut command_open = command_rx.has_changed().is_ok();
    if let Some(reason) = stop_reason {
        return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason,
            bundle_was_accepted: false,
        });
    }

    backend.establish_observation().await?;

    let mut last_submission_started = tokio::time::Instant::now();
    let mut bundle_was_accepted = submit_sponsored_bundle_latching_stop(
        backend,
        signed_raw_transaction,
        tx_hash,
        &mut command_rx,
        &mut command_open,
        &mut stop_reason,
    )
    .await;
    if stop_reason == Some(SponsoredSelfBroadcastStopReason::Shutdown) {
        return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Shutdown,
            bundle_was_accepted,
        });
    }
    if bundle_was_accepted {
        let _ = mark_sponsored_inputs_with_command(
            backend,
            tx_hash,
            &mut command_rx,
            &mut command_open,
            &mut stop_reason,
            None,
        )
        .await?;
    }

    let mut accepted_head_baselines = BTreeMap::new();
    let mut last_observed_head = None;
    let mut next_head_poll = tokio::time::Instant::now();
    let mut heartbeat_deadline = last_submission_started + block_time;
    let mut retry_pending = false;
    let mut pending_lock_renewal = bundle_was_accepted
        .then(|| tokio::time::Instant::now() + SPONSORED_PENDING_LOCK_RENEW_INTERVAL);
    let mut cancel_deadline = (bundle_was_accepted
        && stop_reason == Some(SponsoredSelfBroadcastStopReason::Cancelled))
    .then(|| tokio::time::Instant::now() + cancel_grace);

    loop {
        stop_reason =
            merge_sponsored_stop_reason(stop_reason, sponsored_pending_stop_reason(&command_rx));
        if stop_reason == Some(SponsoredSelfBroadcastStopReason::Shutdown) {
            return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                reason: SponsoredSelfBroadcastStopReason::Shutdown,
                bundle_was_accepted,
            });
        }
        if let Some(reason) = stop_reason
            && !bundle_was_accepted
        {
            return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                reason,
                bundle_was_accepted,
            });
        }
        if stop_reason == Some(SponsoredSelfBroadcastStopReason::Cancelled)
            && cancel_deadline.is_none()
        {
            cancel_deadline = Some(tokio::time::Instant::now() + cancel_grace);
        }

        let now = tokio::time::Instant::now();
        if cancel_deadline.is_some_and(|deadline| now >= deadline) {
            return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                reason: SponsoredSelfBroadcastStopReason::Cancelled,
                bundle_was_accepted,
            });
        }
        if now >= next_head_poll {
            next_head_poll = now + poll_interval;
            let canonical = backend.canonical_state(tx_hash);
            tokio::pin!(canonical);
            let state = tokio::select! {
                state = &mut canonical => Some(state),
                changed = command_rx.changed(), if command_open => {
                    command_open = changed.is_ok();
                    let observed = if changed.is_err() {
                        sponsored_stop_reason(*command_rx.borrow())
                            .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                    } else {
                        sponsored_stop_reason(*command_rx.borrow_and_update())
                            .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                    };
                    stop_reason = merge_sponsored_stop_reason(stop_reason, Some(observed));
                    None
                }
                () = wait_for_sponsored_cancel_deadline(cancel_deadline) => {
                    return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                        reason: SponsoredSelfBroadcastStopReason::Cancelled,
                        bundle_was_accepted,
                    });
                }
            };
            let Some(state) = state else {
                continue;
            };
            if state.confirmation_error.is_some() {
                return Err(eyre!(
                    "privacy-preserving sponsored confirmation is unavailable"
                ));
            }
            if let Some(receipt) = state.receipt {
                return Ok(SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(
                    receipt,
                ));
            }
            if let Some((provider_index, head)) = state.head {
                if let Some(reason) = stop_reason
                    && accepted_head_baselines
                        .get(&provider_index)
                        .is_some_and(|accepted_head| head > *accepted_head)
                {
                    return Ok(SponsoredSelfBroadcastSessionOutcome::Stopped {
                        reason,
                        bundle_was_accepted,
                    });
                }
                accepted_head_baselines
                    .entry(provider_index)
                    .or_insert(head);
                if last_observed_head.is_some_and(|previous| head > previous)
                    && stop_reason.is_none()
                {
                    retry_pending = true;
                    heartbeat_deadline = now + block_time;
                }
                last_observed_head =
                    Some(last_observed_head.map_or(head, |previous| previous.max(head)));
            }
        }

        let now = tokio::time::Instant::now();
        if pending_lock_renewal.is_some_and(|deadline| now >= deadline) {
            let _ = mark_sponsored_inputs_with_command(
                backend,
                tx_hash,
                &mut command_rx,
                &mut command_open,
                &mut stop_reason,
                cancel_deadline,
            )
            .await?;
            pending_lock_renewal = Some(now + SPONSORED_PENDING_LOCK_RENEW_INTERVAL);
            continue;
        }
        if stop_reason.is_none() && now >= heartbeat_deadline {
            retry_pending = true;
            heartbeat_deadline = now + block_time;
        }

        let next_submission = last_submission_started + poll_interval;
        if stop_reason.is_none() && retry_pending && now >= next_submission {
            retry_pending = false;
            last_submission_started = now;
            let accepted = submit_sponsored_bundle_latching_stop(
                backend,
                signed_raw_transaction,
                tx_hash,
                &mut command_rx,
                &mut command_open,
                &mut stop_reason,
            )
            .await;
            heartbeat_deadline = last_submission_started + block_time;
            if accepted {
                bundle_was_accepted = true;
                if stop_reason == Some(SponsoredSelfBroadcastStopReason::Shutdown) {
                    continue;
                }
                let _ = mark_sponsored_inputs_with_command(
                    backend,
                    tx_hash,
                    &mut command_rx,
                    &mut command_open,
                    &mut stop_reason,
                    None,
                )
                .await?;
                pending_lock_renewal =
                    Some(tokio::time::Instant::now() + SPONSORED_PENDING_LOCK_RENEW_INTERVAL);
                accepted_head_baselines.clear();
                if stop_reason == Some(SponsoredSelfBroadcastStopReason::Cancelled) {
                    cancel_deadline = Some(tokio::time::Instant::now() + cancel_grace);
                }
            }
            continue;
        }

        let mut wake_at = if stop_reason.is_some() {
            next_head_poll
        } else if retry_pending {
            next_head_poll.min(heartbeat_deadline).min(next_submission)
        } else {
            next_head_poll.min(heartbeat_deadline)
        };
        if let Some(renewal) = pending_lock_renewal {
            wake_at = wake_at.min(renewal);
        }
        if let Some(deadline) = cancel_deadline {
            wake_at = wake_at.min(deadline);
        }
        tokio::select! {
            () = tokio::time::sleep_until(wake_at) => {}
            changed = command_rx.changed(), if command_open => {
                command_open = changed.is_ok();
                let observed = if changed.is_err() {
                    sponsored_stop_reason(*command_rx.borrow())
                        .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                } else {
                    sponsored_stop_reason(*command_rx.borrow_and_update())
                        .unwrap_or(SponsoredSelfBroadcastStopReason::Shutdown)
                };
                stop_reason = merge_sponsored_stop_reason(stop_reason, Some(observed));
            }
        }
    }
}

async fn wait_for_sponsored_cancel_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

const fn merge_sponsored_stop_reason(
    current: Option<SponsoredSelfBroadcastStopReason>,
    observed: Option<SponsoredSelfBroadcastStopReason>,
) -> Option<SponsoredSelfBroadcastStopReason> {
    match (current, observed) {
        (_, Some(SponsoredSelfBroadcastStopReason::Shutdown)) => {
            Some(SponsoredSelfBroadcastStopReason::Shutdown)
        }
        (Some(current), _) => Some(current),
        (None, observed) => observed,
    }
}

const fn sponsored_stop_reason(
    command: SponsoredSelfBroadcastCommand,
) -> Option<SponsoredSelfBroadcastStopReason> {
    match command {
        SponsoredSelfBroadcastCommand::Running => None,
        SponsoredSelfBroadcastCommand::Cancel => Some(SponsoredSelfBroadcastStopReason::Cancelled),
        SponsoredSelfBroadcastCommand::Shutdown => Some(SponsoredSelfBroadcastStopReason::Shutdown),
    }
}
