use super::{
    helpers::{
        current_unix_seconds, ensure_walletconnect_chain_id_enabled, ethereum_chain_id_hex,
        walletconnect_error_message, walletconnect_is_transient_relay_error,
        walletconnect_namespace_account_support, walletconnect_pairing_expired,
        walletconnect_request_id_seed, walletconnect_request_key_log_label,
        walletconnect_session_expired, walletconnect_session_relay_processable,
        walletconnect_session_request_expiry_timestamp, walletconnect_topic_log_label,
    },
    requests::walletconnect_request_key,
    *,
};

pub(super) fn stop_stale_walletconnect_relay_workers(
    relay_workers: &mut BTreeMap<String, WalletConnectRelayWorkerHandle>,
    active_client_ids: &BTreeSet<String>,
    unsubscribe_first: bool,
) {
    let stale_client_ids = relay_workers
        .keys()
        .filter(|client_id| !active_client_ids.contains(*client_id))
        .cloned()
        .collect::<Vec<_>>();
    for client_id in stale_client_ids {
        if let Some(worker) = relay_workers.remove(&client_id) {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                relay_client_id = %walletconnect_request_key_log_label(&client_id),
                worker_id = worker.worker_id,
                "stopping stale walletconnect relay worker"
            );
            if unsubscribe_first {
                worker.stop_after_unsubscribe();
            } else {
                worker.stop();
            }
        }
    }
}

pub(super) async fn execute_walletconnect_relay_steps(
    worker: &WalletConnectRelayWorkerHandle,
    steps: Vec<WalletConnectRelayStep>,
) -> Result<WalletConnectRelayOutput, String> {
    execute_walletconnect_relay_steps_with_push_wait(worker, steps, true).await
}

pub(super) async fn execute_walletconnect_approval_relay_steps(
    worker: &WalletConnectRelayWorkerHandle,
    store: &DesktopVaultStore,
    view_session: &DesktopViewSession,
    session: &WalletConnectSessionRecord,
    steps: Vec<WalletConnectRelayStep>,
) -> Result<WalletConnectApprovalRelayResult, String> {
    let (pre_persist_steps, post_persist_steps) =
        walletconnect_split_pre_persist_relay_steps(steps);
    let pre_output =
        execute_walletconnect_relay_steps_with_push_wait(worker, pre_persist_steps, false).await?;
    store
        .store_walletconnect_session(view_session, session)
        .map_err(|error| format!("Could not persist WalletConnect session: {error}"))?;
    let (post_output, post_persist_error) = if post_persist_steps.is_empty() {
        (WalletConnectRelayOutput::default(), None)
    } else {
        match execute_walletconnect_relay_steps_with_push_wait(worker, post_persist_steps, false)
            .await
        {
            Ok(output) => (output, None),
            Err(error) => {
                tracing::warn!(
                    target: "wallet::root::walletconnect",
                    session_uuid = %walletconnect_request_key_log_label(&session.session_uuid),
                    error = %error,
                    "walletconnect approval publish failed after session persistence"
                );
                let error = match store.delete_walletconnect_session(&session.session_uuid) {
                    Ok(()) => error,
                    Err(delete_error) => format!(
                        "{error}; could not remove failed WalletConnect session: {delete_error}"
                    ),
                };
                (WalletConnectRelayOutput::default(), Some(error))
            }
        }
    };
    Ok(WalletConnectApprovalRelayResult {
        output: walletconnect_merge_relay_outputs(pre_output, post_output),
        post_persist_error,
    })
}

pub(super) fn walletconnect_split_pre_persist_relay_steps(
    steps: Vec<WalletConnectRelayStep>,
) -> (Vec<WalletConnectRelayStep>, Vec<WalletConnectRelayStep>) {
    let first_publish = steps
        .iter()
        .position(|step| matches!(step, WalletConnectRelayStep::Publish(_)))
        .unwrap_or(steps.len());
    let mut post_persist_steps = steps;
    let pre_persist_steps = post_persist_steps.drain(..first_publish).collect();
    (pre_persist_steps, post_persist_steps)
}

pub(super) fn walletconnect_merge_relay_outputs(
    mut first: WalletConnectRelayOutput,
    second: WalletConnectRelayOutput,
) -> WalletConnectRelayOutput {
    first.messages.extend(second.messages);
    first.subscriptions.extend(second.subscriptions);
    first
}

pub(super) async fn execute_walletconnect_relay_steps_with_push_wait(
    worker: &WalletConnectRelayWorkerHandle,
    steps: Vec<WalletConnectRelayStep>,
    wait_for_push: bool,
) -> Result<WalletConnectRelayOutput, String> {
    execute_walletconnect_relay_steps_with_worker(worker, steps, wait_for_push, false).await
}

pub(super) async fn execute_walletconnect_relay_steps_with_worker(
    worker: &WalletConnectRelayWorkerHandle,
    steps: Vec<WalletConnectRelayStep>,
    wait_for_push: bool,
    emit_pushes: bool,
) -> Result<WalletConnectRelayOutput, String> {
    worker
        .execute(steps, wait_for_push, emit_pushes)
        .await
        .map_err(|_| "WalletConnect relay worker stopped".to_owned())?
}

pub(super) async fn execute_walletconnect_relay_steps_on_socket(
    socket: &mut WalletConnectRelaySocket,
    steps: Vec<WalletConnectRelayStep>,
    wait_for_push: bool,
    emit_pushes: bool,
    event_tx: Option<&mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>>,
) -> Result<WalletConnectRelayOutput, String> {
    let mut output = WalletConnectRelayOutput::default();
    let mut request_id = walletconnect_request_id_seed();
    let command_topics = walletconnect_relay_step_topics(&steps);
    let should_wait_for_push = steps
        .iter()
        .any(|step| matches!(step, WalletConnectRelayStep::Subscribe { .. }))
        && wait_for_push;
    tracing::debug!(
        target: "wallet::root::walletconnect",
        step_count = steps.len(),
        waits_for_push = should_wait_for_push,
        "executing walletconnect relay steps"
    );
    for step in steps {
        request_id = request_id.wrapping_add(1);
        let rpc = step.rpc();
        let response = match socket.request::<Value>(request_id, rpc).await {
            Ok(response) => response,
            Err(error) => {
                let message = walletconnect_error_message(&error);
                walletconnect_route_socket_subscription_payloads(
                    socket,
                    &mut output,
                    event_tx,
                    true,
                    None,
                );
                walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                return Err(message);
            }
        };
        walletconnect_route_socket_subscription_payloads(
            socket,
            &mut output,
            event_tx,
            emit_pushes,
            Some(&command_topics),
        );
        let result = match response.into_result() {
            Ok(result) => result,
            Err(error) => {
                walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                return Err(walletconnect_error_message(&error));
            }
        };
        match step {
            WalletConnectRelayStep::FetchMessages { topic } => {
                let mut fetch_result = result;
                for page in 0..WALLETCONNECT_FETCH_MAX_PAGES {
                    let fetched_messages = relay_messages_from_value(&topic, &fetch_result);
                    let has_more = relay_fetch_response_has_more(&fetch_result);
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        topic = %walletconnect_topic_log_label(&topic),
                        page,
                        message_count = fetched_messages.len(),
                        has_more,
                        "walletconnect relay fetch result"
                    );
                    output.messages.extend(fetched_messages);
                    if !has_more {
                        break;
                    }
                    if walletconnect_fetch_page_limit_exhausted(page, has_more) {
                        walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                        return Err(format!(
                            "WalletConnect relay fetch for {} still has more messages after {} pages",
                            walletconnect_topic_log_label(&topic),
                            WALLETCONNECT_FETCH_MAX_PAGES,
                        ));
                    }
                    request_id = request_id.wrapping_add(1);
                    let response = match socket
                        .request::<Value>(
                            request_id,
                            WalletConnectRelayRpc::FetchMessages {
                                topic: topic.clone(),
                            },
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(error) => {
                            let message = walletconnect_error_message(&error);
                            walletconnect_route_socket_subscription_payloads(
                                socket,
                                &mut output,
                                event_tx,
                                true,
                                None,
                            );
                            walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                            return Err(message);
                        }
                    };
                    walletconnect_route_socket_subscription_payloads(
                        socket,
                        &mut output,
                        event_tx,
                        emit_pushes,
                        Some(&command_topics),
                    );
                    fetch_result = match response.into_result() {
                        Ok(result) => result,
                        Err(error) => {
                            walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                            return Err(walletconnect_error_message(&error));
                        }
                    };
                }
            }
            WalletConnectRelayStep::Subscribe { topic } => {
                if let Some(subscription_id) = relay_subscription_id_from_value(&result) {
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        topic = %walletconnect_topic_log_label(&topic),
                        subscription_id = %walletconnect_request_key_log_label(&subscription_id),
                        "walletconnect relay subscription registered"
                    );
                    output.subscriptions.insert(topic, subscription_id);
                }
            }
            WalletConnectRelayStep::Unsubscribe { topic, .. } => {
                output.subscriptions.remove(&topic);
            }
            WalletConnectRelayStep::Publish(_) => {}
        }
    }
    if should_wait_for_push {
        let messages = match socket
            .collect_subscription_messages(Duration::from_secs(
                WALLETCONNECT_SUBSCRIPTION_PUSH_WAIT_SECS,
            ))
            .await
        {
            Ok(messages) => messages,
            Err(error) => {
                let message = walletconnect_error_message(&error);
                walletconnect_route_socket_subscription_payloads(
                    socket,
                    &mut output,
                    event_tx,
                    true,
                    None,
                );
                walletconnect_emit_accumulated_output_messages(&mut output, event_tx);
                return Err(message);
            }
        };
        tracing::debug!(
            target: "wallet::root::walletconnect",
            message_count = messages.len(),
            "walletconnect relay push wait completed"
        );
        walletconnect_route_subscription_payloads(
            messages,
            &mut output,
            event_tx,
            emit_pushes,
            Some(&command_topics),
        );
    }
    tracing::debug!(
        target: "wallet::root::walletconnect",
        message_count = output.messages.len(),
        subscription_count = output.subscriptions.len(),
        "walletconnect relay steps completed"
    );
    Ok(output)
}

pub(super) fn walletconnect_emit_accumulated_output_messages(
    output: &mut WalletConnectRelayOutput,
    event_tx: Option<&mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>>,
) {
    if event_tx.is_none() || output.messages.is_empty() {
        return;
    }
    let messages = output.messages.split_off(0);
    let _ = walletconnect_emit_subscription_messages(event_tx, messages);
}

pub(super) fn walletconnect_route_socket_subscription_payloads(
    socket: &mut WalletConnectRelaySocket,
    output: &mut WalletConnectRelayOutput,
    event_tx: Option<&mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>>,
    emit_pushes: bool,
    command_topics: Option<&BTreeSet<String>>,
) {
    walletconnect_route_subscription_payloads(
        socket.drain_subscription_messages(),
        output,
        event_tx,
        emit_pushes,
        command_topics,
    );
}

pub(super) fn walletconnect_route_subscription_payloads(
    payloads: Vec<WalletConnectRelaySubscriptionPayload>,
    output: &mut WalletConnectRelayOutput,
    event_tx: Option<&mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>>,
    emit_pushes: bool,
    command_topics: Option<&BTreeSet<String>>,
) {
    let messages = walletconnect_messages_from_subscription_payloads(payloads);
    if messages.is_empty() {
        return;
    }
    if emit_pushes {
        if event_tx.is_some() {
            let _ = walletconnect_emit_subscription_messages(event_tx, messages);
        } else {
            output.messages.extend(messages);
        }
        return;
    }
    if let (Some(event_tx), Some(command_topics)) = (event_tx, command_topics) {
        let mut command_messages = Vec::new();
        let mut push_messages = Vec::new();
        for message in messages {
            if command_topics.contains(&message.topic) {
                command_messages.push(message);
            } else {
                push_messages.push(message);
            }
        }
        let _ = walletconnect_emit_subscription_messages(Some(event_tx), push_messages);
        output.messages.extend(command_messages);
        return;
    }
    output.messages.extend(messages);
}

pub(super) fn walletconnect_emit_subscription_messages(
    event_tx: Option<&mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>>,
    messages: Vec<WalletConnectRelayMessage>,
) -> bool {
    let Some(event_tx) = event_tx else {
        return false;
    };
    if messages.is_empty() {
        return true;
    }
    let _ = event_tx.send(WalletConnectRelayWorkerEvent::Output(
        WalletConnectRelayOutput {
            messages,
            subscriptions: BTreeMap::new(),
        },
    ));
    true
}

pub(super) fn walletconnect_relay_step_topics(
    steps: &[WalletConnectRelayStep],
) -> BTreeSet<String> {
    let mut topics = BTreeSet::new();
    for step in steps {
        match step {
            WalletConnectRelayStep::FetchMessages { topic }
            | WalletConnectRelayStep::Subscribe { topic }
            | WalletConnectRelayStep::Unsubscribe { topic, .. } => {
                topics.insert(topic.clone());
            }
            WalletConnectRelayStep::Publish(rpc) => {
                walletconnect_relay_rpc_topics(rpc, &mut topics);
            }
        }
    }
    topics
}

pub(super) fn walletconnect_relay_rpc_topics(
    rpc: &WalletConnectRelayRpc,
    topics: &mut BTreeSet<String>,
) {
    match rpc {
        WalletConnectRelayRpc::Publish { topic, .. }
        | WalletConnectRelayRpc::FetchMessages { topic }
        | WalletConnectRelayRpc::Subscribe { topic }
        | WalletConnectRelayRpc::Unsubscribe { topic, .. } => {
            topics.insert(topic.clone());
        }
        WalletConnectRelayRpc::BatchFetchMessages {
            topics: batch_topics,
        }
        | WalletConnectRelayRpc::BatchSubscribe {
            topics: batch_topics,
        } => {
            topics.extend(batch_topics.iter().cloned());
        }
    }
}

pub(super) fn walletconnect_messages_from_subscription_payloads(
    payloads: Vec<WalletConnectRelaySubscriptionPayload>,
) -> Vec<WalletConnectRelayMessage> {
    payloads
        .into_iter()
        .map(|payload| WalletConnectRelayMessage {
            topic: payload.topic,
            message: payload.message,
        })
        .collect()
}

pub(super) async fn walletconnect_relay_worker_loop(
    client: WalletConnectRelayClient,
    http: HttpContext,
    mut command_rx: mpsc::UnboundedReceiver<WalletConnectRelayWorkerCommand>,
    event_tx: mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>,
) {
    let mut topics = BTreeSet::<String>::new();
    let mut subscriptions = BTreeMap::<String, String>::new();
    let mut pending_commands = VecDeque::<WalletConnectRelayWorkerCommand>::new();
    let mut reconnect_delay = Duration::from_secs(1);
    loop {
        let mut socket = match walletconnect_relay_worker_connect(
            &client,
            &http,
            &mut command_rx,
            &mut topics,
            &mut pending_commands,
        )
        .await
        {
            Some(Ok(socket)) => socket,
            Some(Err(message)) => {
                let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(message));
                if walletconnect_relay_worker_wait_disconnected_command(
                    &mut command_rx,
                    &mut topics,
                    reconnect_delay,
                )
                .await
                {
                    return;
                }
                reconnect_delay = walletconnect_next_reconnect_delay(reconnect_delay);
                continue;
            }
            None => return,
        };
        reconnect_delay = Duration::from_secs(1);
        let target_topics = topics.clone();
        topics.clear();
        subscriptions.clear();
        match walletconnect_relay_worker_sync_topics(
            &mut socket,
            &mut topics,
            target_topics,
            &mut subscriptions,
            &event_tx,
        )
        .await
        {
            Ok(()) => {
                let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnected);
            }
            Err(error) => {
                let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(error));
                continue;
            }
        }
        let mut reconnect = false;
        while let Some(command) = pending_commands.pop_front() {
            match walletconnect_relay_worker_handle_connected_command(
                &mut socket,
                &mut topics,
                &mut subscriptions,
                &event_tx,
                command,
            )
            .await
            {
                WalletConnectRelayWorkerCommandOutcome::Continue => {}
                WalletConnectRelayWorkerCommandOutcome::Reconnect => {
                    reconnect = true;
                    break;
                }
                WalletConnectRelayWorkerCommandOutcome::Stop => return,
            }
        }
        if reconnect {
            continue;
        }
        loop {
            tokio::select! {
                command = command_rx.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    match walletconnect_relay_worker_handle_connected_command(
                        &mut socket,
                        &mut topics,
                        &mut subscriptions,
                        &event_tx,
                        command,
                    )
                    .await
                    {
                        WalletConnectRelayWorkerCommandOutcome::Continue => {}
                        WalletConnectRelayWorkerCommandOutcome::Reconnect => break,
                        WalletConnectRelayWorkerCommandOutcome::Stop => return,
                    }
                }
                payload = socket.next_subscription_message() => {
                    match payload {
                        Ok(Some(payload)) => {
                            walletconnect_relay_worker_send_output(
                                &event_tx,
                                WalletConnectRelayOutput {
                                    messages: walletconnect_messages_from_subscription_payloads(vec![payload]),
                                    subscriptions: BTreeMap::new(),
                                },
                            );
                        }
                        Ok(None) => {
                            let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(
                                "WalletConnect relay websocket closed".to_owned(),
                            ));
                            break;
                        }
                        Err(error) => {
                            let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(
                                walletconnect_error_message(&error),
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(super) async fn walletconnect_relay_worker_connect(
    client: &WalletConnectRelayClient,
    http: &HttpContext,
    command_rx: &mut mpsc::UnboundedReceiver<WalletConnectRelayWorkerCommand>,
    topics: &mut BTreeSet<String>,
    pending_commands: &mut VecDeque<WalletConnectRelayWorkerCommand>,
) -> Option<Result<WalletConnectRelaySocket, String>> {
    loop {
        tokio::select! {
            result = client.connect(http) => {
                return Some(result.map_err(|error| walletconnect_error_message(&error)));
            }
            command = command_rx.recv() => {
                match command {
                    Some(WalletConnectRelayWorkerCommand::SetTopics { topics: next_topics }) => {
                        *topics = next_topics.into_iter().collect();
                    }
                    Some(command @ WalletConnectRelayWorkerCommand::Execute { .. }) => {
                        pending_commands.push_back(command);
                    }
                    Some(
                        WalletConnectRelayWorkerCommand::Stop
                        | WalletConnectRelayWorkerCommand::StopAfterUnsubscribe,
                    )
                    | None => return None,
                }
            }
        }
    }
}

pub(super) async fn walletconnect_relay_worker_handle_connected_command(
    socket: &mut WalletConnectRelaySocket,
    topics: &mut BTreeSet<String>,
    subscriptions: &mut BTreeMap<String, String>,
    event_tx: &mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>,
    command: WalletConnectRelayWorkerCommand,
) -> WalletConnectRelayWorkerCommandOutcome {
    match command {
        WalletConnectRelayWorkerCommand::SetTopics {
            topics: next_topics,
        } => {
            let next_topics = next_topics.into_iter().collect();
            if next_topics == *topics {
                return WalletConnectRelayWorkerCommandOutcome::Continue;
            }
            if let Err(error) = walletconnect_relay_worker_sync_topics(
                socket,
                topics,
                next_topics,
                subscriptions,
                event_tx,
            )
            .await
            {
                let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(error));
                return WalletConnectRelayWorkerCommandOutcome::Reconnect;
            }
            WalletConnectRelayWorkerCommandOutcome::Continue
        }
        WalletConnectRelayWorkerCommand::Execute {
            steps,
            wait_for_push,
            emit_pushes,
            response_tx,
        } => {
            let unsubscribe_topics = steps
                .iter()
                .filter_map(|step| match step {
                    WalletConnectRelayStep::Unsubscribe { topic, .. } => Some(topic.clone()),
                    WalletConnectRelayStep::FetchMessages { .. }
                    | WalletConnectRelayStep::Subscribe { .. }
                    | WalletConnectRelayStep::Publish(_) => None,
                })
                .collect::<Vec<_>>();
            let mut result = execute_walletconnect_relay_steps_on_socket(
                socket,
                steps,
                wait_for_push,
                emit_pushes,
                Some(event_tx),
            )
            .await;
            let should_reconnect = result
                .as_ref()
                .err()
                .is_some_and(|error| walletconnect_is_transient_relay_error(error));
            if should_reconnect && let Err(error) = &result {
                let _ = event_tx.send(WalletConnectRelayWorkerEvent::Reconnecting(error.clone()));
            }
            if let Ok(output) = &mut result {
                for topic in unsubscribe_topics {
                    subscriptions.remove(&topic);
                }
                subscriptions.extend(output.subscriptions.clone());
            }
            let _ = response_tx.send(result);
            if should_reconnect {
                WalletConnectRelayWorkerCommandOutcome::Reconnect
            } else {
                WalletConnectRelayWorkerCommandOutcome::Continue
            }
        }
        WalletConnectRelayWorkerCommand::StopAfterUnsubscribe => {
            let steps = walletconnect_terminal_unsubscribe_steps(subscriptions);
            if !steps.is_empty()
                && let Err(error) =
                    execute_walletconnect_relay_steps_on_socket(socket, steps, false, false, None)
                        .await
            {
                tracing::warn!(
                    target: "wallet::root::walletconnect",
                    error = %error,
                    "walletconnect terminal unsubscribe failed before worker stop"
                );
            }
            subscriptions.clear();
            topics.clear();
            WalletConnectRelayWorkerCommandOutcome::Stop
        }
        WalletConnectRelayWorkerCommand::Stop => WalletConnectRelayWorkerCommandOutcome::Stop,
    }
}

pub(super) async fn walletconnect_relay_worker_wait_disconnected_command(
    command_rx: &mut mpsc::UnboundedReceiver<WalletConnectRelayWorkerCommand>,
    topics: &mut BTreeSet<String>,
    delay: Duration,
) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        command = command_rx.recv() => {
            match command {
                Some(WalletConnectRelayWorkerCommand::SetTopics { topics: next_topics }) => {
                    *topics = next_topics.into_iter().collect();
                    false
                }
                Some(WalletConnectRelayWorkerCommand::Execute { response_tx, .. }) => {
                    let _ = response_tx.send(Err(
                        "WalletConnect relay is reconnecting; request was not sent".to_owned(),
                    ));
                    false
                }
                Some(
                    WalletConnectRelayWorkerCommand::Stop
                    | WalletConnectRelayWorkerCommand::StopAfterUnsubscribe,
                )
                | None => true,
            }
        }
    }
}

pub(super) async fn walletconnect_relay_worker_sync_topics(
    socket: &mut WalletConnectRelaySocket,
    topics: &mut BTreeSet<String>,
    next_topics: BTreeSet<String>,
    subscriptions: &mut BTreeMap<String, String>,
    event_tx: &mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>,
) -> Result<(), String> {
    let previous_topics = topics.clone();
    let removed_topics = previous_topics
        .difference(&next_topics)
        .cloned()
        .collect::<Vec<_>>();
    let steps =
        walletconnect_relay_topic_resync_steps(&previous_topics, &next_topics, subscriptions);
    *topics = next_topics;
    for topic in &removed_topics {
        subscriptions.remove(topic);
    }
    if steps.is_empty() {
        return Ok(());
    }
    let output =
        execute_walletconnect_relay_steps_on_socket(socket, steps, false, false, Some(event_tx))
            .await?;
    subscriptions.extend(output.subscriptions.clone());
    walletconnect_relay_worker_send_output(event_tx, output);
    Ok(())
}

pub(super) fn walletconnect_relay_worker_send_output(
    event_tx: &mpsc::UnboundedSender<WalletConnectRelayWorkerEvent>,
    output: WalletConnectRelayOutput,
) {
    if output.messages.is_empty() && output.subscriptions.is_empty() {
        return;
    }
    let _ = event_tx.send(WalletConnectRelayWorkerEvent::Output(output));
}

pub(super) fn walletconnect_next_reconnect_delay(current: Duration) -> Duration {
    const MAX_RECONNECT_DELAY_SECS: u64 = 30;
    Duration::from_secs(
        current
            .as_secs()
            .saturating_mul(2)
            .clamp(1, MAX_RECONNECT_DELAY_SECS),
    )
}

pub(super) async fn process_walletconnect_relay_output(
    worker: &WalletConnectRelayWorkerHandle,
    store: &DesktopVaultStore,
    view_session: &DesktopViewSession,
    pairings: &[WalletConnectPairingUri],
    sessions: &[WalletConnectSessionRecord],
    enabled_chain_ids: &BTreeSet<u64>,
    output: WalletConnectRelayOutput,
    now: u64,
) -> WalletConnectRelayProcessingResult {
    let mut result = WalletConnectRelayProcessingResult {
        proposals: Vec::new(),
        removed_pairings: Vec::new(),
        pending_requests: Vec::new(),
        removed_sessions: Vec::new(),
        subscriptions: output.subscriptions,
        error: None,
    };
    tracing::debug!(
        target: "wallet::root::walletconnect",
        message_count = output.messages.len(),
        subscription_count = result.subscriptions.len(),
        pairing_count = pairings.len(),
        session_count = sessions.len(),
        "processing walletconnect relay output"
    );
    for message in output.messages {
        if let Some(pairing) = pairings
            .iter()
            .find(|pairing| pairing.topic == message.topic)
        {
            if walletconnect_pairing_expired(pairing, now) {
                tracing::warn!(
                    target: "wallet::root::walletconnect",
                    pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
                    "ignoring walletconnect proposal for expired pairing URI"
                );
                result.removed_pairings.push(pairing.topic.clone());
                continue;
            }
            match decode_walletconnect_session_proposal(pairing, &message.message) {
                Ok(proposal) => {
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
                        proposal_id = proposal.id,
                        dapp = proposal.peer_metadata.name.as_str(),
                        "decoded walletconnect session proposal"
                    );
                    result.proposals.push(WalletConnectProposalUi {
                        pairing: pairing.clone(),
                        proposal,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
                        error = %walletconnect_error_message(&error),
                        "could not decode walletconnect session proposal"
                    );
                    result.error = Some(format!(
                        "Could not decode WalletConnect proposal: {}",
                        walletconnect_error_message(&error)
                    ));
                }
            }
            continue;
        }
        if let Some(session) = sessions
            .iter()
            .find(|session| session.session_topic == message.topic)
        {
            match process_walletconnect_session_message(
                store,
                view_session,
                session,
                &message,
                enabled_chain_ids,
                now,
            ) {
                Ok(SessionMessageOutcome::Pending(request)) => {
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        request_key = %walletconnect_request_key_log_label(&request.key),
                        session_topic = %walletconnect_topic_log_label(&session.session_topic),
                        method = request.item.method.as_str(),
                        chain_id = request.item.chain_id.as_str(),
                        dapp = request.item.dapp_name.as_str(),
                        "received walletconnect request"
                    );
                    result.pending_requests.push(*request);
                }
                Ok(SessionMessageOutcome::Respond {
                    topic,
                    sym_key,
                    response,
                    response_ttl,
                    response_tag,
                    post_response_requests,
                    removed_session,
                }) => {
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        session_topic = %walletconnect_topic_log_label(&topic),
                        removed_session = removed_session.is_some(),
                        "responding to walletconnect lifecycle/request message"
                    );
                    if let Err(error) = publish_walletconnect_session_response_ref(
                        worker,
                        topic.clone(),
                        &sym_key,
                        *response,
                        response_ttl,
                        response_tag,
                    )
                    .await
                    {
                        result.error = Some(error);
                    }
                    for request in post_response_requests {
                        if let Err(error) = publish_walletconnect_session_request_ref(
                            worker,
                            topic.clone(),
                            &sym_key,
                            request,
                            WC_SESSION_EVENT_REQUEST_TAG,
                        )
                        .await
                        {
                            result.error = Some(error);
                        }
                    }
                    if let Some(session_uuid) = removed_session {
                        result.removed_sessions.push(session_uuid);
                    }
                }
                Ok(SessionMessageOutcome::Ignored) => {
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        session_topic = %walletconnect_topic_log_label(&session.session_topic),
                        "ignored walletconnect session message"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        session_topic = %walletconnect_topic_log_label(&session.session_topic),
                        error = %error,
                        "walletconnect session message processing failed"
                    );
                    result.error = Some(error);
                }
            }
        }
    }
    result
}

pub(super) enum SessionMessageOutcome {
    Pending(Box<WalletConnectRequestUi>),
    Respond {
        topic: String,
        sym_key: [u8; 32],
        response: Box<WalletConnectJsonRpcResponse<Value>>,
        response_ttl: u64,
        response_tag: u32,
        post_response_requests: Vec<WalletConnectJsonRpcRequest<Value>>,
        removed_session: Option<String>,
    },
    Ignored,
}

#[derive(Debug, PartialEq)]
pub(super) enum DecodedSessionJsonRpcMessage {
    Request(WalletConnectJsonRpcRequest<Value>),
    Response,
}

pub(super) fn process_walletconnect_session_message(
    store: &DesktopVaultStore,
    view_session: &DesktopViewSession,
    session: &WalletConnectSessionRecord,
    message: &WalletConnectRelayMessage,
    enabled_chain_ids: &BTreeSet<u64>,
    now: u64,
) -> Result<SessionMessageOutcome, String> {
    let request = match decode_session_jsonrpc_message(session, &message.message)? {
        DecodedSessionJsonRpcMessage::Request(request) => request,
        DecodedSessionJsonRpcMessage::Response => return Ok(SessionMessageOutcome::Ignored),
    };
    tracing::debug!(
        target: "wallet::root::walletconnect",
        session_topic = %walletconnect_topic_log_label(&session.session_topic),
        request_id = request.id,
        method = request.method.as_str(),
        "decoded walletconnect session jsonrpc request"
    );
    match handle_walletconnect_lifecycle_request(request.id, &request.method) {
        WalletConnectLifecycleRequestOutcome::Delete { response } => {
            tracing::info!(
                target: "wallet::root::walletconnect",
                session_uuid = %walletconnect_request_key_log_label(&session.session_uuid),
                session_topic = %walletconnect_topic_log_label(&session.session_topic),
                request_id = request.id,
                "walletconnect session delete received"
            );
            store
                .delete_walletconnect_session(&session.session_uuid)
                .map_err(|error| format!("Could not delete WalletConnect session: {error}"))?;
            return Ok(SessionMessageOutcome::Respond {
                topic: session.session_topic.clone(),
                sym_key: session.keys.sym_key,
                response: Box::new(response),
                response_ttl: WALLETCONNECT_SESSION_DELETE_TTL_SECS,
                response_tag: WC_SESSION_DELETE_RESPONSE_TAG,
                post_response_requests: Vec::new(),
                removed_session: Some(session.session_uuid.clone()),
            });
        }
        WalletConnectLifecycleRequestOutcome::Ping { response } => {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                session_topic = %walletconnect_topic_log_label(&session.session_topic),
                request_id = request.id,
                "walletconnect session ping received"
            );
            let response = if session.lifecycle_state == WalletConnectSessionLifecycleState::Active
                && !walletconnect_session_expired(session, now)
            {
                response
            } else {
                build_walletconnect_jsonrpc_error(
                    request.id,
                    WalletConnectRequestErrorKind::ExpiredRequest,
                    "WalletConnect session has expired",
                )
            };
            return Ok(SessionMessageOutcome::Respond {
                topic: session.session_topic.clone(),
                sym_key: session.keys.sym_key,
                response: Box::new(response),
                response_ttl: WALLETCONNECT_SESSION_PING_TTL_SECS,
                response_tag: WC_SESSION_PING_RESPONSE_TAG,
                post_response_requests: Vec::new(),
                removed_session: None,
            });
        }
        WalletConnectLifecycleRequestOutcome::NotLifecycleRequest => {}
    }
    if request.method != "wc_sessionRequest" {
        tracing::debug!(
            target: "wallet::root::walletconnect",
            session_topic = %walletconnect_topic_log_label(&session.session_topic),
            request_id = request.id,
            method = request.method.as_str(),
            "ignoring unsupported walletconnect session topic method"
        );
        return Ok(SessionMessageOutcome::Ignored);
    }
    let outcome = match process_walletconnect_session_request_message(
        store,
        view_session,
        session,
        message,
        enabled_chain_ids,
        request.id,
        &request.params,
        now,
    ) {
        Ok(outcome) => outcome,
        Err(failure) => {
            tracing::warn!(
                target: "wallet::root::walletconnect",
                session_topic = %walletconnect_topic_log_label(&session.session_topic),
                request_id = request.id,
                error = %failure.message,
                "walletconnect session request rejected"
            );
            return Ok(SessionMessageOutcome::Respond {
                topic: session.session_topic.clone(),
                sym_key: session.keys.sym_key,
                response: Box::new(build_walletconnect_jsonrpc_error(
                    request.id,
                    failure.kind,
                    failure.message,
                )),
                response_ttl: WALLETCONNECT_RELAY_TTL_SECS,
                response_tag: WC_SESSION_REQUEST_RESPONSE_TAG,
                post_response_requests: Vec::new(),
                removed_session: None,
            });
        }
    };
    Ok(outcome)
}

pub(super) fn process_walletconnect_session_request_message(
    store: &DesktopVaultStore,
    view_session: &DesktopViewSession,
    session: &WalletConnectSessionRecord,
    message: &WalletConnectRelayMessage,
    enabled_chain_ids: &BTreeSet<u64>,
    request_id: u64,
    request_params: &Value,
    now: u64,
) -> Result<SessionMessageOutcome, WalletConnectSessionRequestFailure> {
    let chain_id = request_params
        .get("chainId")
        .and_then(Value::as_str)
        .ok_or_else(|| WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::MalformedParams,
            message: "WalletConnect request is missing chainId".to_owned(),
        })?;
    let request_payload =
        request_params
            .get("request")
            .ok_or_else(|| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::MalformedParams,
                message: "WalletConnect request payload is missing".to_owned(),
            })?;
    let method = request_payload
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::MalformedParams,
            message: "WalletConnect request method is missing".to_owned(),
        })?;
    let method_params = request_payload.get("params").unwrap_or(&Value::Null);
    let expiry_timestamp =
        walletconnect_session_request_expiry_timestamp(request_params, request_payload)?;
    let parsed = parse_walletconnect_session_request(request_id, method, method_params)
        .map_err(|error| walletconnect_session_request_failure_from_error(&error))?;
    ensure_walletconnect_chain_id_enabled(chain_id, enabled_chain_ids)?;
    if let WalletConnectParsedRequest::WalletSwitchEthereumChain {
        chain_id: switch_chain,
    } = &parsed
    {
        ensure_walletconnect_chain_id_enabled(
            &format!("eip155:{switch_chain}"),
            enabled_chain_ids,
        )?;
    }
    let resolution = store
        .resolve_walletconnect_session_account(view_session, session)
        .map_err(|error| WalletConnectSessionRequestFailure {
            kind: WalletConnectRequestErrorKind::Internal,
            message: format!("Could not resolve WalletConnect Public account: {error}"),
        })?;
    let selected_account = match &resolution {
        WalletConnectSessionAccountResolution::Usable(account) => account.clone(),
        WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { .. } => {
            return Err(WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "WalletConnect session is paused for a different selected Private wallet"
                    .to_owned(),
            });
        }
        WalletConnectSessionAccountResolution::InvalidPublicAccount => {
            return Err(WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "WalletConnect session Public account is invalid".to_owned(),
            });
        }
    };
    let account_source = selected_account.source;
    let selected_account_support =
        walletconnect_namespace_account_support(&selected_account, Some(view_session));
    let validation = validate_walletconnect_session_request_with_account_support(
        session,
        &resolution,
        selected_account_support,
        &message.topic,
        request_id,
        chain_id,
        parsed.clone(),
        expiry_timestamp,
        now,
    )
    .map_err(|error| walletconnect_session_request_failure_from_error(&error))?;
    if let Some(item) = validation.approval_item {
        tracing::info!(
            target: "wallet::root::walletconnect",
            session_topic = %walletconnect_topic_log_label(&session.session_topic),
            request_id,
            method,
            chain_id,
            dapp = session.peer_metadata.name.as_str(),
            "walletconnect request requires approval"
        );
        return Ok(SessionMessageOutcome::Pending(Box::new(
            WalletConnectRequestUi {
                key: walletconnect_request_key(&message.topic, request_id),
                review_token: walletconnect_request_id_seed(),
                session: session.clone(),
                parsed,
                item,
                account_source,
            },
        )));
    }
    let mut post_response_requests = Vec::new();
    let result = match validation.request {
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts => {
            json!([selected_account.address.to_string()])
        }
        WalletConnectParsedRequest::WalletSwitchEthereumChain {
            chain_id: switch_chain,
        } => {
            let target_chain_id = format!("eip155:{switch_chain}");
            if let Ok(event) = build_walletconnect_session_event(
                session,
                walletconnect_request_id_seed(),
                &target_chain_id,
                "chainChanged",
                json!(ethereum_chain_id_hex(switch_chain)),
            ) {
                post_response_requests.push(event);
            }
            Value::Null
        }
        WalletConnectParsedRequest::PersonalSign { .. }
        | WalletConnectParsedRequest::EthSendTransaction { .. }
        | WalletConnectParsedRequest::EthSignTypedData { .. }
        | WalletConnectParsedRequest::EthSignTypedDataV4 { .. } => Value::Null,
    };
    tracing::debug!(
        target: "wallet::root::walletconnect",
        session_topic = %walletconnect_topic_log_label(&session.session_topic),
        request_id,
        method,
        chain_id,
        "walletconnect request handled without approval"
    );
    Ok(SessionMessageOutcome::Respond {
        topic: session.session_topic.clone(),
        sym_key: session.keys.sym_key,
        response: Box::new(WalletConnectJsonRpcResponse {
            id: request_id,
            jsonrpc: "2.0".to_owned(),
            result: Some(result),
            error: None,
        }),
        response_ttl: WALLETCONNECT_RELAY_TTL_SECS,
        response_tag: WC_SESSION_REQUEST_RESPONSE_TAG,
        post_response_requests,
        removed_session: None,
    })
}

pub(super) fn decode_session_jsonrpc_message(
    session: &WalletConnectSessionRecord,
    encoded: &str,
) -> Result<DecodedSessionJsonRpcMessage, String> {
    let envelope = wallet_ops::WalletConnectEnvelope::from_base64(encoded)
        .map_err(|error| walletconnect_error_message(&error))?;
    let plaintext = decode_walletconnect_message(&session.keys.sym_key, &envelope)
        .map_err(|error| walletconnect_error_message(&error))?;
    let value: Value = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("Could not decode WalletConnect JSON-RPC message: {error}"))?;
    if value.get("method").and_then(Value::as_str).is_some() {
        return serde_json::from_value(value)
            .map(DecodedSessionJsonRpcMessage::Request)
            .map_err(|error| format!("Could not decode WalletConnect JSON-RPC request: {error}"));
    }
    if walletconnect_jsonrpc_response_shape(&value) {
        return Ok(DecodedSessionJsonRpcMessage::Response);
    }
    Err("Could not decode WalletConnect JSON-RPC request: missing field `method`".to_owned())
}

pub(super) fn walletconnect_jsonrpc_response_shape(value: &Value) -> bool {
    value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && value.get("id").is_some()
        && (value.get("result").is_some() || value.get("error").is_some())
}

pub(super) fn walletconnect_client_from_identity(
    project_id: String,
    identity: &WalletConnectRelayIdentity,
) -> WalletConnectRelayClient {
    WalletConnectRelayClient::new(
        WalletConnectRelayConfig { project_id },
        WalletConnectRelayClientAuth::from_signing_key(identity.signing_key),
    )
}

pub(super) fn walletconnect_session_request_failure_from_error(
    error: &WalletConnectError,
) -> WalletConnectSessionRequestFailure {
    let kind = match error {
        WalletConnectError::ExpiredUri => WalletConnectRequestErrorKind::ExpiredRequest,
        WalletConnectError::UnsupportedMethod(_) => {
            WalletConnectRequestErrorKind::UnsupportedMethod
        }
        WalletConnectError::UnsupportedChain(_)
        | WalletConnectError::UnsupportedEvent(_)
        | WalletConnectError::UnsupportedNamespace(_)
        | WalletConnectError::UnsatisfiedNamespaces(_) => {
            WalletConnectRequestErrorKind::UnsupportedChain
        }
        WalletConnectError::InvalidUri(_)
        | WalletConnectError::MalformedParams(_)
        | WalletConnectError::Encode(_) => WalletConnectRequestErrorKind::MalformedParams,
        WalletConnectError::Relay(message)
            if message.contains("account")
                || message.contains("paused")
                || message.contains("not active") =>
        {
            WalletConnectRequestErrorKind::Unauthorized
        }
        WalletConnectError::Crypto | WalletConnectError::Relay(_) | WalletConnectError::Http(_) => {
            WalletConnectRequestErrorKind::Internal
        }
    };
    WalletConnectSessionRequestFailure {
        kind,
        message: walletconnect_error_message(error),
    }
}

pub(super) async fn publish_walletconnect_session_response(
    worker: WalletConnectRelayWorkerHandle,
    topic: String,
    sym_key: [u8; 32],
    response: WalletConnectJsonRpcResponse<Value>,
) -> Result<(), String> {
    publish_walletconnect_session_response_ref(
        &worker,
        topic,
        &sym_key,
        response,
        WALLETCONNECT_RELAY_TTL_SECS,
        WC_SESSION_REQUEST_RESPONSE_TAG,
    )
    .await
}

pub(super) async fn publish_walletconnect_session_response_ref(
    worker: &WalletConnectRelayWorkerHandle,
    topic: String,
    sym_key: &[u8; 32],
    response: WalletConnectJsonRpcResponse<Value>,
    ttl: u64,
    tag: u32,
) -> Result<(), String> {
    let request_id = response.id;
    let response_kind = if response.error.is_some() {
        "error"
    } else {
        "result"
    };
    let topic_label = walletconnect_topic_log_label(&topic);
    tracing::debug!(
        target: "wallet::root::walletconnect",
        topic = %topic_label,
        request_id,
        response_kind,
        "publishing walletconnect session response"
    );
    let message = encode_walletconnect_response_message(sym_key, &response)?;
    let rpc = WalletConnectRelayRpc::Publish {
        topic,
        message,
        ttl,
        tag,
    };
    execute_walletconnect_relay_steps_with_worker(
        worker,
        vec![WalletConnectRelayStep::Publish(rpc)],
        false,
        true,
    )
    .await
    .map(|_| {
        tracing::debug!(
            target: "wallet::root::walletconnect",
            topic = %topic_label,
            request_id,
            response_kind,
            "published walletconnect session response"
        );
    })
}

pub(super) async fn publish_walletconnect_session_request_ref(
    worker: &WalletConnectRelayWorkerHandle,
    topic: String,
    sym_key: &[u8; 32],
    request: WalletConnectJsonRpcRequest<Value>,
    tag: u32,
) -> Result<(), String> {
    let request_id = request.id;
    let method = request.method.clone();
    let topic_label = walletconnect_topic_log_label(&topic);
    tracing::debug!(
        target: "wallet::root::walletconnect",
        topic = %topic_label,
        request_id,
        method = method.as_str(),
        tag,
        "publishing walletconnect session request"
    );
    let plaintext = serde_json::to_vec(&request)
        .map_err(|error| format!("Could not encode WalletConnect request: {error}"))?;
    let message = encode_walletconnect_message(sym_key, &plaintext)
        .map(|envelope| envelope.to_base64())
        .map_err(|error| walletconnect_error_message(&error))?;
    let rpc = WalletConnectRelayRpc::Publish {
        topic,
        message,
        ttl: WALLETCONNECT_RELAY_TTL_SECS,
        tag,
    };
    execute_walletconnect_relay_steps_with_worker(
        worker,
        vec![WalletConnectRelayStep::Publish(rpc)],
        false,
        true,
    )
    .await
    .map(|_| {
        tracing::debug!(
            target: "wallet::root::walletconnect",
            topic = %topic_label,
            request_id,
            method = method.as_str(),
            tag,
            "published walletconnect session request"
        );
    })
}

pub(super) fn encode_walletconnect_response_message(
    sym_key: &[u8; 32],
    response: &WalletConnectJsonRpcResponse<Value>,
) -> Result<String, String> {
    let plaintext = serde_json::to_vec(response)
        .map_err(|error| format!("Could not encode WalletConnect response: {error}"))?;
    encode_walletconnect_message(sym_key, &plaintext)
        .map(|envelope| envelope.to_base64())
        .map_err(|error| walletconnect_error_message(&error))
}

pub(super) fn walletconnect_relay_topic_resync_steps(
    current_topics: &BTreeSet<String>,
    next_topics: &BTreeSet<String>,
    subscriptions: &BTreeMap<String, String>,
) -> Vec<WalletConnectRelayStep> {
    current_topics
        .difference(next_topics)
        .filter_map(|topic| {
            subscriptions
                .get(topic)
                .map(|id| WalletConnectRelayStep::Unsubscribe {
                    topic: topic.clone(),
                    id: id.clone(),
                })
        })
        .chain(walletconnect_relay_sync_steps(next_topics))
        .collect()
}

pub(super) fn walletconnect_terminal_unsubscribe_steps(
    subscriptions: &BTreeMap<String, String>,
) -> Vec<WalletConnectRelayStep> {
    subscriptions
        .iter()
        .map(|(topic, id)| WalletConnectRelayStep::Unsubscribe {
            topic: topic.clone(),
            id: id.clone(),
        })
        .collect()
}

pub(super) fn walletconnect_relay_sync_steps(
    topics: &BTreeSet<String>,
) -> Vec<WalletConnectRelayStep> {
    topics
        .iter()
        .flat_map(|topic| {
            [
                WalletConnectRelayStep::FetchMessages {
                    topic: topic.clone(),
                },
                WalletConnectRelayStep::Subscribe {
                    topic: topic.clone(),
                },
            ]
        })
        .collect()
}

pub(super) fn walletconnect_relay_target_topics(
    pairings: &[WalletConnectPairingUri],
    sessions: &[WalletConnectSessionRecord],
) -> Vec<String> {
    pairings
        .iter()
        .map(|pairing| pairing.topic.clone())
        .chain(sessions.iter().map(|session| session.session_topic.clone()))
        .collect()
}

pub(super) fn walletconnect_active_sessions(
    sessions: &[WalletConnectSessionRecord],
    approval_handoff_sessions: &BTreeMap<String, WalletConnectSessionRecord>,
) -> Vec<WalletConnectSessionRecord> {
    let now = current_unix_seconds();
    let mut seen_topics = BTreeSet::new();
    let mut active_sessions = Vec::new();
    for session in sessions
        .iter()
        .chain(approval_handoff_sessions.values())
        .filter(|session| walletconnect_session_relay_processable(session, now))
    {
        if seen_topics.insert(session.session_topic.clone()) {
            active_sessions.push(session.clone());
        }
    }
    active_sessions
}

pub(super) fn walletconnect_active_sessions_for_relay_client(
    sessions: &[WalletConnectSessionRecord],
    approval_handoff_sessions: &BTreeMap<String, WalletConnectSessionRecord>,
    client_id: &str,
) -> Vec<WalletConnectSessionRecord> {
    walletconnect_active_sessions(sessions, approval_handoff_sessions)
        .into_iter()
        .filter(|session| session.relay_client_id.as_str() == client_id)
        .collect()
}

pub(super) fn relay_messages_from_value(
    default_topic: &str,
    value: &Value,
) -> Vec<WalletConnectRelayMessage> {
    let values = value
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| value.as_array().cloned())
        .unwrap_or_default();
    values
        .into_iter()
        .filter_map(|value| {
            let message = value.get("message")?.as_str()?.to_owned();
            let topic = value
                .get("topic")
                .and_then(Value::as_str)
                .unwrap_or(default_topic)
                .to_owned();
            Some(WalletConnectRelayMessage { topic, message })
        })
        .collect()
}

pub(super) fn relay_fetch_response_has_more(value: &Value) -> bool {
    value
        .get("hasMore")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) const fn walletconnect_fetch_page_limit_exhausted(page: usize, has_more: bool) -> bool {
    has_more && page.saturating_add(1) == WALLETCONNECT_FETCH_MAX_PAGES
}

pub(super) fn relay_subscription_id_from_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("subscriptionId").and_then(Value::as_str))
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(str::to_owned)
}
