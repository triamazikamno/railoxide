use super::super::helpers::parse_caip2_chain_id;
use super::*;
use wallet_ops::settings::{CustomTokenSettings, resolve_effective_chain_rpc_route};

impl WalletRoot {
    pub(super) fn load_gateway_asset_metadata(&mut self, cx: &Context<'_, Self>) {
        let requests: Vec<_> = self
            .walletconnect
            .pending_requests
            .values()
            .filter(|request| {
                request.request_control.is_some()
                    && matches!(
                        request.parsed,
                        WalletConnectParsedRequest::WalletWatchAsset { .. }
                    )
            })
            .filter(|request| {
                !self
                    .walletconnect
                    .watch_asset_metadata
                    .contains_key(&request.key)
            })
            .cloned()
            .collect();
        for request in requests {
            let WalletConnectParsedRequest::WalletWatchAsset { address, .. } = request.parsed
            else {
                continue;
            };
            let Some(chain_id) = parse_caip2_chain_id(&request.item.chain_id) else {
                continue;
            };
            let key = request.key.clone();
            if let Some(token) = self.effective_token_registry.get(chain_id, &address) {
                self.walletconnect.watch_asset_metadata.insert(
                    key,
                    Some(Ok(CustomTokenSettings {
                        chain_id,
                        token_address: address.to_string(),
                        symbol: token.symbol.clone(),
                        decimals: token.decimals,
                        icon_path: token.icon_path.clone(),
                        price_anchor: token.price_anchor.clone(),
                    })),
                );
                continue;
            }
            self.walletconnect
                .watch_asset_metadata
                .insert(key.clone(), None);
            let endpoint = resolve_effective_chain_rpc_route(
                chain_id,
                self.effective_chain_configs.get(&chain_id),
            )
            .ok()
            .and_then(|route| route.endpoints().first().cloned());
            let control = request.request_control.clone().expect("gateway request");
            let review_token = request.review_token;
            let join = self.runtime.spawn(async move {
                let endpoint = endpoint.ok_or(wallet_ops::RpcBrokerError::NoEndpoint { chain_id })?;
                let reads = request.rpc_reads.as_ref().ok_or(wallet_ops::RpcBrokerError::OriginRejected)?;
                tokio::select! {
                    biased;
                    () = control.cancelled() => Err(wallet_ops::RpcBrokerError::OriginRejected),
                    result = wallet_ops::gateway::policy::watch_asset_metadata(reads, &control, endpoint, chain_id, address) => result,
                }
            });
            cx.spawn(async move |this, cx| {
                let result = join.await;
                let _ = this.update(cx, |root, cx| {
                    if !root
                        .walletconnect
                        .pending_requests
                        .get(&key)
                        .is_some_and(|request| {
                            request.review_token == review_token && request.is_current()
                        })
                    {
                        return;
                    }
                    let metadata = result.ok().and_then(Result::ok).ok_or_else(|| {
                        "Token metadata is unavailable. The token has not been added.".to_owned()
                    });
                    root.walletconnect
                        .watch_asset_metadata
                        .insert(key, Some(metadata));
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(super) fn approve_gateway_policy(
        &mut self,
        request: WalletConnectRequestUi,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let token = if matches!(
            request.parsed,
            WalletConnectParsedRequest::WalletWatchAsset { .. }
        ) {
            if let Some(Ok(token)) = self
                .walletconnect
                .watch_asset_metadata
                .get(&request.key)
                .and_then(Option::as_ref)
            {
                Some(token.clone())
            } else {
                self.walletconnect.error = Some(Arc::from(
                    "Token metadata is unavailable. Wait for metadata before confirming.",
                ));
                cx.notify();
                return;
            }
        } else {
            None
        };
        let sender = match self.walletconnect_response_sender(&request, cx) {
            Ok(sender) => sender,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let key = request.key.clone();
        let active_wallet_generation = self.active_wallet_generation;
        self.walletconnect.request_actions.insert(key.clone());
        self.walletconnect.error = None;
        let begin = self
            .runtime
            .spawn(async move { sender.begin_approval().await.map(|()| sender) });
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |this, cx| {
            let begun = begin.await;
            let Ok(Ok(sender)) = begun else {
                let _ = this.update_in(cx, |root, _, cx| {
                    root.walletconnect.request_actions.remove(&key);
                    root.walletconnect.error = Some(Arc::from(
                        "Approval could not start. Review and confirm again.",
                    ));
                    cx.notify();
                });
                return;
            };
            let persisted = this.update_in(cx, |root, _, cx| -> Result<(), String> {
                if !root.root_replacement_is_allowed()
                    || !root
                        .walletconnect
                        .pending_requests
                        .get(&key)
                        .is_some_and(|current| {
                            current.review_token == request.review_token && current.is_current()
                        })
                {
                    return Err("This request is no longer available.".to_owned());
                }
                if let Some(token) = token {
                    let editor = root
                        .settings_editor
                        .clone()
                        .ok_or_else(|| "Token settings are unavailable.".to_owned())?;
                    let control = request
                        .request_control
                        .as_ref()
                        .ok_or_else(|| "This request is no longer available.".to_owned())?;
                    let registry = editor
                        .update(cx, |editor, cx| editor.add_dapp_token(token, control, cx))?;
                    root.effective_token_registry = registry;
                    root.publish_gateway_desktop_state();
                }
                Ok(())
            });
            if !matches!(persisted, Ok(Ok(()))) {
                let message = persisted
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "This request is no longer available.".to_owned());
                let _ = runtime
                    .spawn(async move { sender.return_to_review().await })
                    .await;
                let _ = this.update_in(cx, |root, _, cx| {
                    root.walletconnect.request_actions.remove(&key);
                    root.walletconnect.error = Some(Arc::from(message));
                    cx.notify();
                });
                return;
            }
            let result = runtime
                .spawn(async move { sender.send(Ok(Value::Null)).await })
                .await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&key);
                root.walletconnect.remove_pending_request(&key);
                if root.walletconnect.request_dialog_key.as_deref() == Some(key.as_str()) {
                    root.clear_walletconnect_request_dialog_state(window, cx);
                    window.close_dialog(cx);
                }
                if !matches!(result, Ok(Ok(()))) {
                    root.walletconnect.error =
                        Some(Arc::from("The browser request is no longer available."));
                } else if let WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } =
                    request.parsed
                    && root.active_wallet_generation == active_wallet_generation
                    && root.root_replacement_is_allowed()
                    && root.view_session.is_some()
                    && root.effective_chain_configs.contains_key(&chain_id)
                {
                    root.select_chain(chain_id, window, cx);
                }
                root.sync_walletconnect_attention();
                cx.notify();
            });
        })
        .detach();
    }
}
