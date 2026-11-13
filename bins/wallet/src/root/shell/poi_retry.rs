use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::{Arc, Weak};

use gpui::Context;
use wallet_ops::PoiArtifactCacheAttemptId;

use super::super::{ChainUtxoState, WalletRoot};

#[derive(Default)]
pub(in crate::root) struct PoiArtifactCacheRetryAttempts {
    active: BTreeMap<u64, PoiArtifactCacheRetryAttempt>,
    next_request_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PoiArtifactCacheRetryAttempt {
    request_token: Option<u64>,
    attempt_id: Option<PoiArtifactCacheAttemptId>,
}

enum PoiArtifactCacheRetryTaskEvent {
    AdmissionFailed(String),
    Admitted(PoiArtifactCacheAttemptId),
    Finished {
        attempt_id: PoiArtifactCacheAttemptId,
        result: Result<(), String>,
    },
}

impl PoiArtifactCacheRetryAttempts {
    pub(in crate::root) fn begin(&mut self, chain_id: u64) -> Option<u64> {
        let request_token = self.next_request_token.checked_add(1)?;
        match self.active.entry(chain_id) {
            Entry::Occupied(_) => None,
            Entry::Vacant(entry) => {
                self.next_request_token = request_token;
                entry.insert(PoiArtifactCacheRetryAttempt {
                    request_token: Some(request_token),
                    attempt_id: None,
                });
                Some(request_token)
            }
        }
    }

    pub(in crate::root) fn bind(
        &mut self,
        chain_id: u64,
        request_token: u64,
        attempt_id: PoiArtifactCacheAttemptId,
    ) -> bool {
        let Some(attempt) = self.active.get_mut(&chain_id) else {
            return false;
        };
        if attempt.request_token != Some(request_token) || attempt.attempt_id.is_some() {
            return false;
        }
        attempt.request_token = None;
        attempt.attempt_id = Some(attempt_id);
        true
    }

    pub(in crate::root) fn cancel_pending(&mut self, chain_id: u64, request_token: u64) -> bool {
        match self.active.entry(chain_id) {
            Entry::Occupied(entry)
                if entry.get().request_token == Some(request_token)
                    && entry.get().attempt_id.is_none() =>
            {
                entry.remove();
                true
            }
            Entry::Occupied(_) | Entry::Vacant(_) => false,
        }
    }

    pub(in crate::root) fn finish(
        &mut self,
        chain_id: u64,
        attempt_id: PoiArtifactCacheAttemptId,
    ) -> bool {
        match self.active.entry(chain_id) {
            Entry::Occupied(entry) if entry.get().attempt_id == Some(attempt_id) => {
                entry.remove();
                true
            }
            Entry::Occupied(_) | Entry::Vacant(_) => false,
        }
    }

    pub(in crate::root) fn contains(&self, chain_id: u64) -> bool {
        self.active.contains_key(&chain_id)
    }

    pub(in crate::root) fn clear(&mut self) {
        self.active.clear();
    }
}

impl WalletRoot {
    pub(super) fn retry_selected_poi_artifact_cache_refresh(&mut self, cx: &mut Context<'_, Self>) {
        let chain_id = self.last_evm_chain;
        let Some(request_token) = self.poi_artifact_cache_retry_attempts.begin(chain_id) else {
            return;
        };
        cx.notify();

        let Some(session) = self
            .chain_states
            .get(&chain_id)
            .and_then(ChainUtxoState::poi_refresh_session)
        else {
            self.poi_artifact_cache_retry_attempts
                .cancel_pending(chain_id, request_token);
            cx.notify();
            return;
        };
        let retry_wallet_generation = self.active_wallet_generation;
        let retry_session = Arc::downgrade(&session);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = self.runtime.spawn(async move {
            let retry = match session.retry_poi_artifact_cache().await {
                Ok(retry) => retry,
                Err(error) => {
                    let _ = event_tx.send(PoiArtifactCacheRetryTaskEvent::AdmissionFailed(
                        format!("{error:#}"),
                    ));
                    return;
                }
            };
            let attempt_id = retry.attempt_id();
            if event_tx
                .send(PoiArtifactCacheRetryTaskEvent::Admitted(attempt_id))
                .is_err()
            {
                return;
            }
            let result = retry
                .wait()
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            let _ = event_tx.send(PoiArtifactCacheRetryTaskEvent::Finished { attempt_id, result });
        });
        self.wallet_sync_lifecycle.track_wallet_task(task);
        cx.spawn(async move |root, cx| {
            while let Some(event) = event_rx.recv().await {
                match event {
                    PoiArtifactCacheRetryTaskEvent::AdmissionFailed(error) => {
                        let _ = root.update(cx, |root, cx| {
                            let attempt_released = root
                                .poi_artifact_cache_retry_attempts
                                .cancel_pending(chain_id, request_token);
                            if attempt_released {
                                cx.notify();
                            }
                            let session_is_current = root
                                .chain_states
                                .get(&chain_id)
                                .and_then(ChainUtxoState::poi_refresh_session)
                                .is_some_and(|current| {
                                    Weak::ptr_eq(&Arc::downgrade(&current), &retry_session)
                                });
                            if attempt_released
                                && ppoi_retry_completion_is_current(
                                    root.active_wallet_generation,
                                    retry_wallet_generation,
                                    session_is_current,
                                )
                            {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to admit PPOI corpus refresh retry"
                                );
                            }
                        });
                    }
                    PoiArtifactCacheRetryTaskEvent::Admitted(attempt_id) => {
                        let _ = root.update(cx, |root, cx| {
                            if root.poi_artifact_cache_retry_attempts.bind(
                                chain_id,
                                request_token,
                                attempt_id,
                            ) {
                                cx.notify();
                            }
                        });
                    }
                    PoiArtifactCacheRetryTaskEvent::Finished { attempt_id, result } => {
                        let _ = root.update(cx, |root, cx| {
                            let attempt_released = root
                                .poi_artifact_cache_retry_attempts
                                .finish(chain_id, attempt_id);
                            if attempt_released {
                                cx.notify();
                            }
                            let session_is_current = root
                                .chain_states
                                .get(&chain_id)
                                .and_then(ChainUtxoState::poi_refresh_session)
                                .is_some_and(|current| {
                                    Weak::ptr_eq(&Arc::downgrade(&current), &retry_session)
                                });
                            if !attempt_released
                                || !ppoi_retry_completion_is_current(
                                    root.active_wallet_generation,
                                    retry_wallet_generation,
                                    session_is_current,
                                )
                            {
                                return;
                            }
                            if let Err(error) = result {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to retry PPOI corpus refresh"
                                );
                            }
                        });
                    }
                }
            }
        })
        .detach();
    }
}

pub(in crate::root) const fn ppoi_retry_completion_is_current(
    current_wallet_generation: u64,
    retry_wallet_generation: u64,
    session_is_current: bool,
) -> bool {
    current_wallet_generation == retry_wallet_generation && session_is_current
}
