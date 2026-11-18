use super::*;

impl WalletRoot {
    pub(in crate::root) fn start_staking_refresh(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.tab != GovernanceTab::Staking || self.selected_evm_chain().is_none() {
            return;
        }
        self.clean_governance_participants();
        let key = self.governance_context_key();
        let generation = self.governance.staking.begin(key.clone());
        let chain_id = key.chain_id;
        let participant_ids = key.participants.clone();
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let mut tokens = governance_contracts(chain_id)
            .map(|contracts| {
                contracts
                    .reward_tokens
                    .iter()
                    .map(|token| token.token)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        tokens.sort();
        cx.spawn(async move |this, cx| {
            let general = governance_route(
                chain_id,
                &effective_chain,
                wallet_ops::RpcTrafficClass::General,
            );
            let global = match &general {
                Ok(route) => fetch_staking_global_metrics(chain_id, route, &http)
                    .await
                    .map_err(|error| Arc::from(error.to_string()))
                    .and_then(|metrics| {
                        metrics.ok_or_else(|| Arc::from("Staking is not deployed on this chain"))
                    }),
                Err(error) => Err(Arc::from(error.as_str())),
            };
            let global_metrics = global.as_ref().ok().cloned();
            let Ok(accepted) = this.update(cx, |root, cx| {
                let accepted = root
                    .governance
                    .staking
                    .apply_global(&key, generation, global);
                cx.notify();
                accepted
            }) else {
                return;
            };
            if !accepted {
                return;
            }
            let (Some(metrics), Ok(general)) = (global_metrics, general) else {
                return;
            };
            let metadata = fetch_interval_metadata(chain_id, &tokens, &general, &http)
                .await
                .ok()
                .flatten();
            let countdown = metadata
                .as_ref()
                .and_then(|metadata| reward_interval_countdown(metadata, metrics.chain_time));
            let Ok(accepted) = this.update(cx, |root, cx| {
                let accepted = root
                    .governance
                    .staking
                    .apply_reward_interval(&key, generation, countdown);
                cx.notify();
                accepted
            }) else {
                return;
            };
            if !accepted {
                return;
            }
            if participant_ids.is_empty() {
                let _ = this.update(cx, |root, cx| {
                    let finished = root.governance.staking.finish(&key, generation);
                    if finished {
                        root.start_governance_time_tick(generation, cx);
                    }
                    cx.notify();
                });
                return;
            }
            let executor = cx.background_executor().clone();
            let current_cx = cx.clone();
            let chunk_size = wallet_ops::MulticallChunkSize::default();
            read_participants_isolated(
                participant_ids,
                async || executor.timer(isolated_read_jitter()).await,
                || {
                    this.read_with(&current_cx, |root, _| {
                        root.governance.staking.current(&key, generation)
                    })
                    .unwrap_or(false)
                },
                async || open_isolated_governance_route(chain_id, &effective_chain, &http).await,
                async |participant: GovernanceParticipantIdentity, route| {
                    let route = match route {
                        Ok(route) => route,
                        Err(error) => {
                            let _ = this.update(&mut *cx, |root, cx| {
                                root.governance.staking.fail_participant(
                                    &key,
                                    generation,
                                    participant.uuid.clone(),
                                    &tokens,
                                    Arc::from(error),
                                );
                                cx.notify();
                            });
                            return;
                        }
                    };
                    let account = fetch_account_stakes(
                        chain_id,
                        participant.address,
                        metrics.chain_time,
                        &route,
                        &http,
                        chunk_size,
                    )
                    .await
                    .map_err(|error| Arc::<str>::from(error.to_string()));
                    let _ = this.update(&mut *cx, |root, cx| {
                        root.governance.staking.apply_account(
                            &key,
                            generation,
                            participant.uuid.clone(),
                            account,
                        );
                        cx.notify();
                    });
                    let snapshots = fetch_account_snapshots(
                        chain_id,
                        participant.address,
                        &route,
                        &http,
                        chunk_size,
                    )
                    .await
                    .map_err(|error| Arc::<str>::from(error.to_string()));
                    let per_token = async {
                        match (metadata.as_ref(), &snapshots) {
                            (Some(metadata), Ok(snapshots)) => fetch_reward_evidence_multi(
                                chain_id,
                                participant.address,
                                &tokens,
                                metadata,
                                snapshots,
                                &route,
                                &http,
                                chunk_size,
                            )
                            .await
                            .map_err(|error| Arc::<str>::from(error.to_string())),
                            (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                            (_, Err(error)) => Err(Arc::clone(error)),
                        }
                    };
                    let bulk = async {
                        match (metadata.as_ref(), &snapshots) {
                            (Some(metadata), Ok(snapshots)) => fetch_reward_batch_evidence(
                                chain_id,
                                participant.address,
                                &tokens,
                                metadata,
                                snapshots,
                                &route,
                                &http,
                                chunk_size,
                            )
                            .await
                            .map_err(|error| Arc::<str>::from(error.to_string())),
                            (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                            (_, Err(error)) => Err(Arc::clone(error)),
                        }
                    };
                    let (rewards, evidence) = futures_util::future::join(per_token, bulk).await;
                    drop(route);
                    let _ = this.update(&mut *cx, |root, cx| {
                        for (token_index, &token) in tokens.iter().enumerate() {
                            let reward = match &rewards {
                                Ok(results) => results.get(token_index).map_or_else(
                                    || Err(Arc::from("Reward result is missing")),
                                    |result| result.evidence.clone().map_err(Arc::from),
                                ),
                                Err(error) => Err(Arc::clone(error)),
                            };
                            root.governance.staking.apply_reward(
                                &key,
                                generation,
                                participant.uuid.clone(),
                                token,
                                reward,
                            );
                        }
                        root.governance.staking.apply_bulk_reward(
                            &key,
                            generation,
                            participant.uuid.clone(),
                            tokens.clone(),
                            &evidence,
                        );
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                let finished = root.governance.staking.finish(&key, generation);
                if finished {
                    root.start_governance_time_tick(generation, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_governance_time_tick(&mut self, generation: u64, cx: &Context<'_, Self>) {
        if generation != self.governance.staking.generation
            || self.active_activity != crate::root::sidebar::Activity::Proposals
            || self.governance.tab != GovernanceTab::Staking
        {
            return;
        }
        let (owner, armed) =
            arm_governance_time_tick(self.governance.participant_time_tick_generation, generation);
        self.governance.participant_time_tick_generation = owner;
        if !armed {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GOVERNANCE_TIME_TICK).await;
            let _ = this.update(cx, |root, cx| {
                let (owner, claimed) = claim_governance_time_tick(
                    root.governance.participant_time_tick_generation,
                    generation,
                );
                root.governance.participant_time_tick_generation = owner;
                if !claimed {
                    return;
                }
                if generation != root.governance.staking.generation
                    || root.governance.tab != GovernanceTab::Staking
                    || root.active_activity != crate::root::sidebar::Activity::Proposals
                {
                    return;
                }
                let crossed =
                    root.governance.staking.chain_time().is_some_and(|now| {
                        governance_boundary_crossed(&root.governance.staking, now)
                    });
                if crossed {
                    root.start_staking_refresh(cx);
                } else {
                    root.start_governance_time_tick(generation, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn sync_governance_participant_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = self
            .governance_participants()
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect::<Vec<_>>();
        let selected = values.iter().cloned().collect();
        let items = participant_choices(
            &self.public_accounts,
            &selected,
            self.selected_wallet_id.as_deref(),
            "",
        );
        self.governance.sync_participant_picker(
            self.selected_wallet_id.as_deref().map(str::to_owned),
            items,
            &values,
            window,
            cx,
        );
    }
}
