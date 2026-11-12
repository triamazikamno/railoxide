use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{AsyncApp, Context, WeakEntity};
use tokio::sync::{oneshot, watch};
use wallet_ops::{
    ListUtxosOutput, PoiArtifactCacheProgress, SyncProgressUpdate, WalletPpoiWorkflowStatus,
    WalletReadiness, WalletSession, WalletSessionObservation, WalletSyncTip,
};

use super::super::utxo::should_focus_utxo_table;
use super::super::{
    InitialCatchUpFingerprint, InitialSyncObservation, WalletRoot, format_report_chain,
};
use super::lifecycle::{InstalledObserverCompletion, InstalledObserverToken};
use super::status::syncing_progress;
use super::{
    ChainProgressProjection, ChainSyncProgress, ChainUtxoState, WalletReadinessDisposition,
    chain_progress_update_is_current, ppoi_validation_completion_is_current,
    retain_auxiliary_stream, wallet_readiness_disposition,
};

/// Identity of one chain-load attempt, shared by its progress and observer tasks.
#[derive(Clone)]
pub(super) struct ChainLoadScope {
    pub(super) chain_id: u64,
    pub(super) wallet_id: Arc<str>,
    pub(super) wallet_generation: u64,
    pub(super) lifecycle_generation: u64,
    pub(super) task_id: u64,
    pub(super) observer_token: InstalledObserverToken,
}

impl WalletRoot {
    pub(super) fn spawn_chain_progress_forwarder(
        scope: ChainLoadScope,
        mut progress_rx: watch::Receiver<Option<SyncProgressUpdate>>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                if progress_rx.changed().await.is_err() {
                    break;
                }
                let progress = *progress_rx.borrow();
                let should_continue = this.update(cx, |root, cx| {
                    root.apply_chain_load_progress(&scope, progress, cx)
                });
                if !matches!(should_continue, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_chain_load_progress(
        &mut self,
        scope: &ChainLoadScope,
        progress: Option<SyncProgressUpdate>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        let projection = self
            .chain_states
            .get(&chain_id)
            .and_then(|state| match state {
                ChainUtxoState::Loading { .. } => Some(ChainProgressProjection::Loading),
                ChainUtxoState::Syncing { observer_token, .. } => {
                    Some(ChainProgressProjection::Syncing {
                        token: observer_token,
                    })
                }
                ChainUtxoState::Ready { observer_token, .. } => {
                    Some(ChainProgressProjection::Ready {
                        token: observer_token,
                    })
                }
                ChainUtxoState::Idle | ChainUtxoState::Error { .. } => None,
            });
        let startup_is_current = matches!(projection, Some(ChainProgressProjection::Loading))
            && self.is_current_chain_load_startup(
                scope.wallet_id.as_ref(),
                scope.wallet_generation,
                chain_id,
                scope.lifecycle_generation,
                scope.task_id,
            );
        if !chain_progress_update_is_current(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            startup_is_current,
            projection,
            &scope.observer_token,
        ) {
            return false;
        }
        let fingerprint = match self.chain_states.get_mut(&chain_id) {
            Some(ChainUtxoState::Loading { progress: state }) => {
                state.observe(progress);
                Some(InitialCatchUpFingerprint::new(progress, None))
            }
            Some(ChainUtxoState::Syncing {
                progress: state,
                sync_tip,
                ..
            }) => {
                // Once the session is installed, a commitment update can describe
                // the forest's own live catch-up instead of this sync's remaining
                // work, so the percent leaves it out as the stage display does. A
                // chain that was synced a moment ago then doesn't read as half done.
                // The percent can lag until the next wallet update because of it.
                let progress = syncing_progress(progress);
                state.observe(progress);
                Some(InitialCatchUpFingerprint::new(progress, Some(*sync_tip)))
            }
            Some(ChainUtxoState::Ready { .. }) => None,
            Some(ChainUtxoState::Idle | ChainUtxoState::Error { .. }) | None => {
                return false;
            }
        };
        if let Some(fingerprint) = fingerprint {
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Progress(fingerprint),
            );
        }
        cx.notify();
        true
    }

    pub(super) fn spawn_chain_session_observer(
        scope: ChainLoadScope,
        previous_start_block: Option<u64>,
        result_rx: oneshot::Receiver<eyre::Result<WalletSession>>,
        mut observer_cancel_rx: watch::Receiver<bool>,
        observer_completed_tx: watch::Sender<bool>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            Self::observe_chain_session(
                &this,
                cx,
                &scope,
                previous_start_block,
                result_rx,
                &mut observer_cancel_rx,
                observer_completed_tx,
            )
            .await;
        })
        .detach();
    }

    async fn observe_chain_session(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
        scope: &ChainLoadScope,
        previous_start_block: Option<u64>,
        result_rx: oneshot::Receiver<eyre::Result<WalletSession>>,
        observer_cancel_rx: &mut watch::Receiver<bool>,
        observer_completed_tx: watch::Sender<bool>,
    ) {
        let _observer_completion = InstalledObserverCompletion(observer_completed_tx);
        let chain_id = scope.chain_id;
        let session = match result_rx.await {
            Ok(Ok(session)) => Arc::new(session),
            Ok(Err(error)) => {
                let _ = this.update(cx, |root, cx| {
                    root.fail_chain_load_startup(
                        scope,
                        previous_start_block,
                        || {
                            let message = format_report_chain(&error);
                            tracing::error!(
                                chain_id,
                                error = %message,
                                "wallet chain sync startup failed"
                            );
                            Arc::from(message)
                        },
                        cx,
                    );
                });
                return;
            }
            Err(error) => {
                let _ = this.update(cx, |root, cx| {
                    root.fail_chain_load_startup(
                        scope,
                        previous_start_block,
                        || Arc::from(format!("wallet UTXO task failed: {error}")),
                        cx,
                    );
                });
                return;
            }
        };

        let is_current = this
            .update(cx, |root, _cx| {
                root.chain_load_startup_is_still_current(scope)
            })
            .unwrap_or(false);
        if !is_current {
            if let Err(error) = session.stop().await {
                tracing::warn!(chain_id, %error, "failed to stop stale wallet sync session");
            }
            return;
        }

        let mut observation_rx = session.observation_rx.clone();
        let initial_observation = observation_rx.borrow_and_update().clone();
        let sync_tip_rx = Some(session.sync_tip_rx.clone());
        let poi_refreshing_rx = Some(session.poi_refreshing_rx.clone());
        let poi_artifact_cache_progress_rx = session.poi_artifact_cache_progress_rx.clone();
        let initial_snapshot = initial_observation.snapshot.clone();
        let initial_readiness = wallet_readiness_disposition(&initial_observation.readiness);
        let initial_ppoi_workflow_status = initial_observation.ppoi_workflow_status;
        let initial_sync_tip = *session.sync_tip_rx.borrow();
        let initial_poi_refreshing = *session.poi_refreshing_rx.borrow();
        let initial_poi_artifact_cache_progress = poi_artifact_cache_progress_rx
            .as_ref()
            .and_then(|rx| rx.borrow().get(&chain_id).cloned());

        let installed = this.update(cx, |root, cx| {
            root.install_chain_session(
                scope,
                &session,
                &initial_snapshot,
                &initial_readiness,
                initial_sync_tip,
                initial_poi_refreshing,
                initial_ppoi_workflow_status,
                initial_poi_artifact_cache_progress,
                cx,
            )
        });
        if !matches!(installed, Ok(true)) {
            if let Err(error) = session.stop().await {
                tracing::warn!(chain_id, %error, "failed to stop stale wallet sync session");
            }
            return;
        }
        if matches!(initial_observation.readiness, WalletReadiness::Failed(_)) {
            if let Err(error) = session.stop().await {
                tracing::warn!(chain_id, %error, "failed to stop failed wallet sync session");
            }
            return;
        }
        if initial_observation.readiness == WalletReadiness::Shutdown {
            let _ = this.update(cx, |root, _cx| {
                root.invalidate_block_for_current_observer(
                    scope.wallet_id.as_ref(),
                    scope.wallet_generation,
                    chain_id,
                    &scope.observer_token,
                );
            });
            return;
        }

        Self::observe_installed_chain_session(
            this,
            cx,
            scope,
            &session,
            observer_cancel_rx,
            observation_rx,
            sync_tip_rx,
            poi_refreshing_rx,
            poi_artifact_cache_progress_rx,
            initial_ppoi_workflow_status,
        )
        .await;
    }

    fn fail_chain_load_startup(
        &mut self,
        scope: &ChainLoadScope,
        previous_start_block: Option<u64>,
        message: impl FnOnce() -> Arc<str>,
        cx: &mut Context<'_, Self>,
    ) {
        let chain_id = scope.chain_id;
        let is_current = self.is_current_chain_load_startup(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            scope.lifecycle_generation,
            scope.task_id,
        );
        self.finish_chain_load_startup(chain_id, scope.lifecycle_generation, scope.task_id);
        if !is_current {
            return;
        }
        self.http.rpc_broker().invalidate_block(chain_id);
        self.chain_states.insert(
            chain_id,
            ChainUtxoState::Error {
                message: message(),
                start_block: previous_start_block,
                ppoi_workflow_status: WalletPpoiWorkflowStatus::default(),
            },
        );
        self.handle_initial_sync_observation(
            scope.wallet_generation,
            chain_id,
            InitialSyncObservation::Error,
        );
        if self.last_evm_chain == chain_id {
            self.sync_utxo_table(cx);
        }
        cx.notify();
    }

    fn chain_load_startup_is_still_current(&mut self, scope: &ChainLoadScope) -> bool {
        let is_current = self.is_current_chain_load_startup(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            scope.chain_id,
            scope.lifecycle_generation,
            scope.task_id,
        );
        if !is_current {
            self.finish_chain_load_startup(
                scope.chain_id,
                scope.lifecycle_generation,
                scope.task_id,
            );
        }
        is_current
    }

    fn install_chain_session(
        &mut self,
        scope: &ChainLoadScope,
        session: &Arc<WalletSession>,
        initial_snapshot: &Arc<ListUtxosOutput>,
        initial_readiness: &WalletReadinessDisposition,
        initial_sync_tip: WalletSyncTip,
        initial_poi_refreshing: bool,
        initial_ppoi_workflow_status: WalletPpoiWorkflowStatus,
        initial_poi_artifact_cache_progress: Option<PoiArtifactCacheProgress>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        if !self.is_current_chain_load_startup(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            scope.lifecycle_generation,
            scope.task_id,
        ) {
            self.finish_chain_load_startup(chain_id, scope.lifecycle_generation, scope.task_id);
            return false;
        }
        self.wallet_sync_lifecycle
            .finish_startup_after_session_installation(
                chain_id,
                scope.lifecycle_generation,
                scope.task_id,
            );
        // The sync's percent carries over from the startup.
        let mut progress = match self.chain_states.get(&chain_id) {
            Some(
                ChainUtxoState::Loading { progress } | ChainUtxoState::Syncing { progress, .. },
            ) => *progress,
            Some(
                ChainUtxoState::Idle | ChainUtxoState::Ready { .. } | ChainUtxoState::Error { .. },
            )
            | None => ChainSyncProgress::default(),
        };
        progress.observe(syncing_progress(progress.latest()));
        let state = match initial_readiness.clone() {
            WalletReadinessDisposition::Ready => ChainUtxoState::Ready {
                snapshot: initial_snapshot.clone(),
                session: session.clone(),
                observer_token: scope.observer_token.clone(),
                sync_tip: initial_sync_tip,
                poi_refreshing: initial_poi_refreshing,
                ppoi_workflow_status: initial_ppoi_workflow_status,
            },
            WalletReadinessDisposition::Syncing => ChainUtxoState::Syncing {
                snapshot: initial_snapshot.clone(),
                progress,
                catching_up: false,
                session: session.clone(),
                observer_token: scope.observer_token.clone(),
                sync_tip: initial_sync_tip,
                poi_refreshing: initial_poi_refreshing,
                ppoi_workflow_status: initial_ppoi_workflow_status,
            },
            WalletReadinessDisposition::Error(message) => ChainUtxoState::Error {
                message,
                start_block: Some(session.start_block),
                ppoi_workflow_status: initial_ppoi_workflow_status,
            },
        };
        let initially_ready = matches!(&state, ChainUtxoState::Ready { .. });
        let initially_failed = matches!(&state, ChainUtxoState::Error { .. });
        let initial_sync_fingerprint = match &state {
            ChainUtxoState::Syncing {
                progress, sync_tip, ..
            } => Some(InitialCatchUpFingerprint::new(
                progress.latest(),
                Some(*sync_tip),
            )),
            ChainUtxoState::Idle
            | ChainUtxoState::Loading { .. }
            | ChainUtxoState::Ready { .. }
            | ChainUtxoState::Error { .. } => None,
        };
        // A session can install with a head already observed, and the
        // sync-tip stream only reports later transitions. Seed the broker
        // here so its block epoch is established before the first read.
        let installed_head_block = state.sync_tip().and_then(|tip| tip.head_block);
        self.chain_states.insert(chain_id, state);
        if let Some(installed_head_block) = installed_head_block {
            self.http
                .rpc_broker()
                .notify_block(chain_id, installed_head_block);
        }
        if let Some(fingerprint) = initial_sync_fingerprint {
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Progress(fingerprint),
            );
        }
        if initially_ready {
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Ready,
            );
            self.init_wallet_activity_last_seen(chain_id);
        }
        if initially_failed {
            self.http.rpc_broker().invalidate_block(chain_id);
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Error,
            );
        }
        if let Some(progress) = initial_poi_artifact_cache_progress {
            self.poi_artifact_cache_progress.insert(chain_id, progress);
        } else {
            self.poi_artifact_cache_progress.remove(&chain_id);
        }
        if self.last_evm_chain == chain_id {
            self.sync_utxo_table(cx);
            self.focus_utxo_table_on_render = should_focus_utxo_table(
                self.active_activity,
                self.active_wallet_tab,
                self.chain_states.get(&chain_id),
            );
        }
        cx.notify();
        true
    }

    async fn observe_installed_chain_session(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
        scope: &ChainLoadScope,
        session: &Arc<WalletSession>,
        observer_cancel_rx: &mut watch::Receiver<bool>,
        mut observation_rx: watch::Receiver<WalletSessionObservation>,
        mut sync_tip_rx: Option<watch::Receiver<WalletSyncTip>>,
        mut poi_refreshing_rx: Option<watch::Receiver<bool>>,
        mut poi_artifact_cache_progress_rx: Option<
            watch::Receiver<BTreeMap<u64, PoiArtifactCacheProgress>>,
        >,
        mut last_ppoi_workflow_status: WalletPpoiWorkflowStatus,
    ) {
        let chain_id = scope.chain_id;
        loop {
            tokio::select! {
                changed = observer_cancel_rx.changed() => {
                    if changed.is_err() || *observer_cancel_rx.borrow() {
                        let _ = this.update(cx, |root, _cx| {
                            root.invalidate_block_for_current_observer(
                                scope.wallet_id.as_ref(),
                                scope.wallet_generation,
                                chain_id,
                                &scope.observer_token,
                            );
                        });
                        if let Err(error) = session.stop().await {
                            tracing::warn!(
                                chain_id,
                                %error,
                                "failed to stop cancelled wallet sync observer session"
                            );
                        }
                        break;
                    }
                }
                changed = observation_rx.changed() => {
                    if changed.is_err() {
                        let session_to_stop = this
                            .update(cx, |root, cx| {
                                root.transition_current_installed_observer_to_error(
                                    scope.wallet_id.as_ref(),
                                    scope.wallet_generation,
                                    chain_id,
                                    scope.lifecycle_generation,
                                    scope.task_id,
                                    &scope.observer_token,
                                    None,
                                    Arc::from("wallet session observation stream closed"),
                                    cx,
                                )
                            })
                            .ok()
                            .flatten();
                        if let Some(session_to_stop) = session_to_stop
                            && let Err(error) = session_to_stop.stop().await
                        {
                            tracing::warn!(
                                chain_id,
                                %error,
                                "failed to stop wallet sync session after observation stream closure"
                            );
                        }
                        break;
                    }
                    let observation = observation_rx.borrow_and_update().clone();
                    let latest_poi_refreshing = poi_refreshing_rx
                        .as_ref()
                        .map(|rx| *rx.borrow());
                    let ppoi_workflow_status = observation.ppoi_workflow_status;
                    let validation_completed = ppoi_validation_completion_is_current(
                        last_ppoi_workflow_status,
                        ppoi_workflow_status,
                        true,
                    );
                    let disposition = wallet_readiness_disposition(&observation.readiness);
                    if let WalletReadinessDisposition::Error(message) = &disposition {
                        let session_to_stop = this
                            .update(cx, |root, cx| {
                                root.transition_chain_observer_to_error(
                                    scope,
                                    ppoi_workflow_status,
                                    message,
                                    validation_completed,
                                    cx,
                                )
                            })
                            .ok()
                            .flatten();
                        if let Some(session_to_stop) = session_to_stop
                            && let Err(error) = session_to_stop.stop().await
                        {
                            tracing::warn!(
                                chain_id,
                                %error,
                                "failed to stop terminal wallet sync session"
                            );
                        }
                        break;
                    }
                    let ready = matches!(disposition, WalletReadinessDisposition::Ready);
                    let snapshot = observation.snapshot;
                    let should_continue = this.update(cx, |root, cx| {
                        root.apply_chain_observation(
                            scope,
                            ready,
                            &snapshot,
                            latest_poi_refreshing,
                            ppoi_workflow_status,
                            validation_completed,
                            cx,
                        )
                    });
                    if !matches!(should_continue, Ok(true)) {
                        break;
                    }
                    last_ppoi_workflow_status = ppoi_workflow_status;
                }
                changed = async {
                    match sync_tip_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if !retain_auxiliary_stream(&mut sync_tip_rx, &changed) {
                        let _ = this.update(cx, |root, _cx| {
                            root.invalidate_block_for_current_observer(
                                scope.wallet_id.as_ref(),
                                scope.wallet_generation,
                                chain_id,
                                &scope.observer_token,
                            );
                        });
                        continue;
                    }
                    let sync_tip = *sync_tip_rx
                        .as_ref()
                        .expect("sync-tip receiver is present after successful change")
                        .borrow();
                    let should_continue = this.update(cx, |root, cx| {
                        root.apply_chain_sync_tip(scope, sync_tip, cx)
                    });
                    if !matches!(should_continue, Ok(true)) {
                        break;
                    }
                }
                changed = async {
                    match poi_refreshing_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if !retain_auxiliary_stream(&mut poi_refreshing_rx, &changed) {
                        continue;
                    }
                    let poi_refreshing = *poi_refreshing_rx
                        .as_ref()
                        .expect("POI-refresh receiver is present after successful change")
                        .borrow();
                    let should_continue = this.update(cx, |root, cx| {
                        root.apply_chain_poi_refreshing(scope, poi_refreshing, cx)
                    });
                    if !matches!(should_continue, Ok(true)) {
                        break;
                    }
                }
                changed = async {
                    match poi_artifact_cache_progress_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if !retain_auxiliary_stream(&mut poi_artifact_cache_progress_rx, &changed) {
                        continue;
                    }
                    let progress = poi_artifact_cache_progress_rx
                        .as_ref()
                        .and_then(|rx| rx.borrow().get(&chain_id).cloned());
                    let should_continue = this.update(cx, |root, cx| {
                        root.apply_chain_poi_artifact_progress(scope, progress, cx)
                    });
                    if !matches!(should_continue, Ok(true)) {
                        break;
                    }
                }
            }
        }

        let _ = this.update(cx, |root, _cx| {
            root.finish_chain_load_startup(chain_id, scope.lifecycle_generation, scope.task_id);
        });
    }

    fn transition_chain_observer_to_error(
        &mut self,
        scope: &ChainLoadScope,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
        message: &Arc<str>,
        validation_completed: bool,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<WalletSession>> {
        let session = self.transition_current_installed_observer_to_error(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            scope.chain_id,
            scope.lifecycle_generation,
            scope.task_id,
            &scope.observer_token,
            Some(ppoi_workflow_status),
            Arc::clone(message),
            cx,
        );
        if session.is_some() && validation_completed {
            self.pending_ppoi_validation_toast =
                Some((Arc::clone(&scope.wallet_id), scope.wallet_generation));
        }
        session
    }

    fn apply_chain_observation(
        &mut self,
        scope: &ChainLoadScope,
        ready: bool,
        snapshot: &Arc<ListUtxosOutput>,
        latest_poi_refreshing: Option<bool>,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
        validation_completed: bool,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        if !self.is_current_installed_observer(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            &scope.observer_token,
        ) {
            return false;
        }
        let Some(state) = self.chain_states.remove(&chain_id) else {
            return false;
        };
        let became_ready = ready && matches!(&state, ChainUtxoState::Syncing { .. });
        let mut state = match state {
            ChainUtxoState::Syncing {
                progress,
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ..
            } if ready => {
                self.finish_chain_load_startup(chain_id, scope.lifecycle_generation, scope.task_id);
                let _ = progress;
                ChainUtxoState::Ready {
                    snapshot: snapshot.clone(),
                    session,
                    observer_token,
                    sync_tip,
                    poi_refreshing,
                    ppoi_workflow_status,
                }
            }
            ChainUtxoState::Syncing {
                progress,
                catching_up,
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ..
            } => ChainUtxoState::Syncing {
                snapshot: snapshot.clone(),
                progress,
                catching_up,
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ppoi_workflow_status,
            },
            ChainUtxoState::Ready {
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ..
            } if ready => ChainUtxoState::Ready {
                snapshot: snapshot.clone(),
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ppoi_workflow_status,
            },
            ChainUtxoState::Ready {
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ..
            } => ChainUtxoState::Syncing {
                snapshot: snapshot.clone(),
                progress: ChainSyncProgress::default(),
                catching_up: true,
                session,
                observer_token,
                sync_tip,
                poi_refreshing,
                ppoi_workflow_status,
            },
            state @ (ChainUtxoState::Idle
            | ChainUtxoState::Loading { .. }
            | ChainUtxoState::Error { .. }) => {
                self.chain_states.insert(chain_id, state);
                return false;
            }
        };
        if let Some(latest_poi_refreshing) = latest_poi_refreshing {
            state.set_poi_refreshing(latest_poi_refreshing);
        }
        self.chain_states.insert(chain_id, state);
        if ready {
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Ready,
            );
            self.init_wallet_activity_last_seen(chain_id);
        }
        if validation_completed {
            self.pending_ppoi_validation_toast =
                Some((Arc::clone(&scope.wallet_id), scope.wallet_generation));
        }
        self.refresh_open_form_assets_for_snapshot(snapshot, cx);
        self.refresh_private_swap_assets(chain_id, cx);
        if became_ready {
            self.reschedule_ready_public_broadcaster_cost_estimates(chain_id, cx);
        }
        if self.last_evm_chain == chain_id {
            self.sync_utxo_table(cx);
        }
        cx.notify();
        true
    }

    fn apply_chain_sync_tip(
        &mut self,
        scope: &ChainLoadScope,
        sync_tip: WalletSyncTip,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        if !self.is_current_installed_observer(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            &scope.observer_token,
        ) {
            return false;
        }
        let has_installed_chain_state = self.chain_states.get(&chain_id).is_some_and(|state| {
            matches!(
                state,
                ChainUtxoState::Syncing { .. } | ChainUtxoState::Ready { .. }
            )
        });
        if has_installed_chain_state && let Some(new_head_block) = sync_tip.head_block {
            self.http
                .rpc_broker()
                .notify_block(chain_id, new_head_block);
        }
        let Some(state) = self.chain_states.get_mut(&chain_id) else {
            return false;
        };
        let fingerprint = match state {
            ChainUtxoState::Syncing {
                sync_tip: state, ..
            } => {
                *state = sync_tip;
                Some(InitialCatchUpFingerprint::new(None, Some(sync_tip)))
            }
            ChainUtxoState::Ready {
                sync_tip: state, ..
            } => {
                *state = sync_tip;
                None
            }
            ChainUtxoState::Idle
            | ChainUtxoState::Loading { .. }
            | ChainUtxoState::Error { .. } => return false,
        };
        if let Some(fingerprint) = fingerprint {
            self.handle_initial_sync_observation(
                scope.wallet_generation,
                chain_id,
                InitialSyncObservation::Progress(fingerprint),
            );
        }
        if self.last_evm_chain == chain_id {
            self.sync_utxo_finality_context(cx);
        }
        cx.notify();
        true
    }

    fn apply_chain_poi_refreshing(
        &mut self,
        scope: &ChainLoadScope,
        poi_refreshing: bool,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        if !self.is_current_installed_observer(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            &scope.observer_token,
        ) {
            return false;
        }
        let Some(state) = self.chain_states.get_mut(&chain_id) else {
            return false;
        };
        if !matches!(
            state,
            ChainUtxoState::Syncing { .. } | ChainUtxoState::Ready { .. }
        ) {
            return false;
        }
        state.set_poi_refreshing(poi_refreshing);
        if self.last_evm_chain == chain_id {
            self.sync_utxo_poi_refreshing(poi_refreshing, cx);
        }
        cx.notify();
        true
    }

    fn apply_chain_poi_artifact_progress(
        &mut self,
        scope: &ChainLoadScope,
        progress: Option<PoiArtifactCacheProgress>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let chain_id = scope.chain_id;
        if !self.is_current_installed_observer(
            scope.wallet_id.as_ref(),
            scope.wallet_generation,
            chain_id,
            &scope.observer_token,
        ) {
            return false;
        }
        if let Some(progress) = progress {
            self.poi_artifact_cache_progress.insert(chain_id, progress);
        } else {
            self.poi_artifact_cache_progress.remove(&chain_id);
        }
        cx.notify();
        true
    }
}
