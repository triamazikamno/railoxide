use super::*;

/// How a submission reports its progress and route, and how long it waits for a response.
pub(crate) struct PublicBroadcasterDelivery {
    pub(crate) progress_tx: Option<TransactionGenerationProgressSender>,
    pub(crate) route_tx: Option<SubmissionRouteSender>,
    pub(crate) timeout: Duration,
    pub(crate) republish_interval: Duration,
}

pub(in crate::desktop) async fn submit_public_broadcaster_plan<P>(
    waku: &PublicBroadcasterWaku,
    transaction: TransactionRequest,
    mut prepared: PreparedPublicBroadcasterPlan<P>,
    payload_inputs: Option<Vec<SelectedInputIdentity>>,
    delivery: PublicBroadcasterDelivery,
) -> Result<PublicBroadcasterSubmissionResult> {
    let broadcaster = &prepared.broadcaster;
    tracing::info!(target: LOG_TARGET,
        chain_id = broadcaster.chain_id,
        broadcaster = %broadcaster.railgun_address,
        broadcaster_identifier = ?broadcaster.identifier.as_deref(),
        fees_id = %broadcaster.fees_id,
        token = ?broadcaster.token,
        fee_amount = %prepared.fee_amount,
        gas_limit = prepared.gas_limit,
        min_gas_price = prepared.min_gas_price,
        bound_min_gas_price = prepared.bound_min_gas_price,
        data_len = transaction.input.input().map_or(0, |input| input.len()),
        "preparing public broadcaster transact request"
    );
    let result = submit_public_broadcaster_transaction(
        waku,
        transaction,
        std::mem::take(&mut prepared.pre_transaction_pois_per_txid_leaf_per_list),
        &prepared.broadcaster,
        prepared.bound_min_gas_price,
        delivery,
    )
    .await?;

    Ok(prepared.into_submission_result(result, payload_inputs))
}

/// Publish an admitted complete transaction and its POIs through the existing
/// encrypted request/response transport, on a Waku node of the submission's own.
/// Dropping the future stops local retries and retires the node through its teardown delay,
/// which a closed scope ends at once. Every exit leaves the route `Closed`.
pub(crate) async fn submit_public_broadcaster_transaction(
    waku: &PublicBroadcasterWaku,
    transaction: TransactionRequest,
    pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
    broadcaster: &PublicBroadcasterCandidate,
    bound_min_gas_price: u128,
    delivery: PublicBroadcasterDelivery,
) -> Result<PublicBroadcasterResultKind> {
    let PublicBroadcasterDelivery {
        progress_tx,
        route_tx,
        timeout,
        republish_interval,
    } = delivery;
    let _closing = ClosingSubmissionRoute(route_tx.clone());
    let transact_topic = transact_topic(broadcaster.chain_id);
    let response_topic = transact_response_topic(broadcaster.chain_id);
    update_transaction_generation_stage(
        progress_tx.as_ref(),
        TransactionGenerationStage::PublishingToBroadcaster,
    );
    let params = public_broadcaster_transact_params(
        broadcaster,
        transaction,
        bound_min_gas_price,
        pre_transaction_pois_per_txid_leaf_per_list,
    )?;
    let encrypt_started = Instant::now();
    let encrypted = EncryptedTransactRequest::encrypt(broadcaster.viewing_public_key, &params)
        .wrap_err("encrypt public broadcaster transact request")?;
    let payload = encrypted
        .to_transact_payload()
        .wrap_err("serialize public broadcaster transact request")?;
    tracing::info!(target: LOG_TARGET,
        chain_id = broadcaster.chain_id,
        broadcaster = %broadcaster.railgun_address,
        fees_id = %broadcaster.fees_id,
        payload_len = payload.len(),
        elapsed_ms = encrypt_started.elapsed().as_millis(),
        "built public broadcaster encrypted Waku payload"
    );
    update_submission_route(route_tx.as_ref(), |route| {
        route.state = SubmissionRouteState::Connecting;
    });
    let retiring = RetiringNode {
        waku,
        node: Some(waku.start_node().await?),
    };
    let (node, operation) = retiring.get();
    let peers = node.route_health().peers;
    update_submission_route(route_tx.as_ref(), |route| {
        route.state = SubmissionRouteState::Ready;
        route.peers = peers;
    });
    deliver_public_broadcaster_request(
        node,
        operation,
        &encrypted.shared_key,
        &payload,
        broadcaster,
        &transact_topic,
        &response_topic,
        progress_tx.as_ref(),
        route_tx.as_ref(),
        timeout,
        republish_interval,
    )
    .await
}

/// Retires a started submission node and its operation client however its submission ends,
/// including when the submission's future is dropped.
struct RetiringNode<'a> {
    waku: &'a PublicBroadcasterWaku,
    /// Taken only on drop.
    node: Option<(Box<dyn SubmissionNode>, OperationHttpClient)>,
}

impl RetiringNode<'_> {
    fn get(&self) -> (&dyn SubmissionNode, &OperationHttpClient) {
        let (node, operation) = self.node.as_ref().expect("the node is taken only on drop");
        (node.as_ref(), operation)
    }
}

impl Drop for RetiringNode<'_> {
    fn drop(&mut self) {
        if let Some((node, operation)) = self.node.take() {
            self.waku.retire(node, operation);
        }
    }
}

/// Subscribe to the response topic, publish, and until a response or `timeout` republish
/// while the route is ready. A route lost in built-in Tor mode is recovered on new circuits
/// with the same node, a bounded number of times. Returned errors all occur before
/// publication is attempted; a failed publish may still have reached peers, so it enters the
/// response-waiting lifecycle.
async fn deliver_public_broadcaster_request(
    node: &dyn SubmissionNode,
    operation: &OperationHttpClient,
    shared_key: &[u8; 32],
    payload: &[u8],
    broadcaster: &PublicBroadcasterCandidate,
    transact_topic: &str,
    response_topic: &str,
    progress_tx: Option<&TransactionGenerationProgressSender>,
    route_tx: Option<&SubmissionRouteSender>,
    timeout: Duration,
    republish_interval: Duration,
) -> Result<PublicBroadcasterResultKind> {
    let pubsub_path = node.pubsub_path();
    tracing::info!(target: LOG_TARGET,
        pubsub_path = %pubsub_path,
        response_topic = %response_topic,
        "subscribing to public broadcaster response topic"
    );
    let subscribe_started = Instant::now();
    let mut response_rx = node
        .subscribe(vec![response_topic.to_owned()])
        .await
        .wrap_err("subscribe to public broadcaster response topic")?;
    tracing::info!(target: LOG_TARGET,
        response_topic = %response_topic,
        elapsed_ms = subscribe_started.elapsed().as_millis(),
        "subscribed to public broadcaster response topic"
    );
    if let Err(error) =
        publish_public_broadcaster_payload(node, pubsub_path, transact_topic, payload, 1).await
    {
        tracing::warn!(target: LOG_TARGET,
            %error,
            attempt = 1,
            "publish initial public broadcaster transact request failed"
        );
    }
    let published = tokio::time::Instant::now();
    let deadline = published + timeout;
    let mut publishes = 1u32;
    update_submission_route(route_tx, |route| route.publishes = publishes);
    update_transaction_generation_stage(
        progress_tx,
        TransactionGenerationStage::WaitingForBroadcasterResponse,
    );

    let republish = move |attempt: usize| async move {
        if let Err(error) =
            publish_public_broadcaster_payload(node, pubsub_path, transact_topic, payload, attempt)
                .await
        {
            tracing::warn!(target: LOG_TARGET, %error, attempt, "republish public broadcaster transact request failed");
        }
    };
    let recover = move |attempt: u32| async move {
        tracing::info!(target: LOG_TARGET,
            attempt,
            peer_id = ?node.local_peer_id(),
            "recovering public broadcaster submission route"
        );
        if let Err(error) = operation.reisolate() {
            tracing::warn!(target: LOG_TARGET, %error, attempt, "re-isolate public broadcaster submission route failed");
        }
        node.refresh_route();
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        node.wait_ready(SUBMISSION_NODE_READY_TIMEOUT.min(left))
            .await
            .is_ok()
    };
    // Polled alongside the response and timeout, so a pending publish or recovery delays
    // neither, and returning drops them.
    let mut publishing: Option<BoxFuture<'_, ()>> = None;
    let mut recovering: Option<BoxFuture<'_, bool>> = None;
    let recoverable = operation.isolation() == OperationNetworkIsolation::Dedicated;
    let mut recoveries_used = 0u32;
    let mut not_ready_since: Option<tokio::time::Instant> = None;
    let mut attempt = 1usize;
    let mut next_republish = published + republish_interval;
    let mut poll = tokio::time::interval_at(
        published + SUBMISSION_ROUTE_POLL_INTERVAL,
        SUBMISSION_ROUTE_POLL_INTERVAL,
    );
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);
    let result = loop {
        let (mut publish_now, mut recover_now) = (false, false);
        tokio::select! {
            () = &mut sleep => {
                tracing::warn!(target: LOG_TARGET,
                    chain_id = broadcaster.chain_id,
                    broadcaster = %broadcaster.railgun_address,
                    fees_id = %broadcaster.fees_id,
                    response_topic = %response_topic,
                    timeout_ms = timeout.as_millis(),
                    "timed out waiting for public broadcaster response"
                );
                break PublicBroadcasterResultKind::TimedOut;
            },
            msg = response_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::warn!(target: LOG_TARGET, response_topic = %response_topic, "public broadcaster response channel closed");
                    break PublicBroadcasterResultKind::TimedOut;
                };
                tracing::info!(target: LOG_TARGET,
                    content_topic = %msg.content_topic,
                    payload_len = msg.payload.len(),
                    "received public broadcaster response candidate"
                );
                match decode_public_broadcaster_response(shared_key, &msg.payload) {
                    Ok(Some(result)) => {
                        // Detailed errors can contain the broadcaster's RPC data.
                        // Show them in progress without copying them into wallet logs.
                        tracing::info!(target: LOG_TARGET,
                            submitted = matches!(result, PublicBroadcasterResultKind::Submitted { .. }),
                            "decrypted public broadcaster response"
                        );
                        break result;
                    }
                    Ok(None) => tracing::debug!(target: LOG_TARGET, "public broadcaster response was not decryptable with request key"),
                    Err(error) => tracing::debug!(target: LOG_TARGET, %error, "ignoring undecryptable public broadcaster response"),
                }
            }
            () = async { publishing.as_mut().expect("polled while publishing").await },
                if publishing.is_some() => publishing = None,
            ready = async { recovering.as_mut().expect("polled while recovering").await },
                if recovering.is_some() =>
            {
                recovering = None;
                let now = tokio::time::Instant::now();
                tracing::info!(target: LOG_TARGET, attempt = recoveries_used, ready, "public broadcaster submission route recovery ended");
                if ready {
                    not_ready_since = None;
                    let peers = node.route_health().peers;
                    update_submission_route(route_tx, |route| {
                        route.state = SubmissionRouteState::Ready;
                        route.peers = peers;
                    });
                    // A publish pending since before the loss would hold off the resend;
                    // its attempt was already counted.
                    publishing = None;
                    publish_now = true;
                    next_republish = now + republish_interval;
                    poll.reset_at(now + SUBMISSION_ROUTE_POLL_INTERVAL);
                } else {
                    recover_now = recoveries_used < SUBMISSION_ROUTE_RECOVERY_LIMIT && now < deadline;
                }
            }
            now = poll.tick() => {
                let health = node.route_health();
                let ready = health.ready();
                not_ready_since = if ready { None } else { Some(not_ready_since.unwrap_or(now)) };
                let recovery_running = recovering.is_some();
                update_submission_route(route_tx, |route| {
                    route.peers = health.peers;
                    if !recovery_running {
                        route.state = if ready {
                            SubmissionRouteState::Ready
                        } else {
                            SubmissionRouteState::Reconnecting
                        };
                    }
                });
                if !recovery_running {
                    publish_now = ready && publishing.is_none() && now >= next_republish;
                    if publish_now {
                        next_republish = now + republish_interval;
                    }
                    recover_now = recoverable
                        && recoveries_used < SUBMISSION_ROUTE_RECOVERY_LIMIT
                        && not_ready_since
                            .is_some_and(|since| now.duration_since(since) >= SUBMISSION_ROUTE_LOSS_GRACE);
                }
            }
        }
        if publish_now {
            attempt = attempt.saturating_add(1);
            publishes = publishes.saturating_add(1);
            update_submission_route(route_tx, |route| route.publishes = publishes);
            publishing = Some(Box::pin(republish(attempt)));
        }
        if recover_now {
            recoveries_used += 1;
            update_submission_route(route_tx, |route| {
                route.state = SubmissionRouteState::Reconnecting;
                route.recoveries_used = recoveries_used;
            });
            recovering = Some(Box::pin(recover(recoveries_used)));
        }
    };

    Ok(result)
}
