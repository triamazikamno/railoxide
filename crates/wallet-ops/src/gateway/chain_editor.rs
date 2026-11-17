//! Volatile, authenticated editor views. Endpoint-bearing replies never enter UI snapshots.
use super::{DappProvider, Delivery, GatewayWalletState, PeerId, valid_id};
use crate::gateway::GatewayServerMessage;
use railgun_ui::chain_editor::{ChainEditorCommand, ChainEditorSnapshot, NativeUsdProbe};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayChainEditorCommand {
    Open {
        view_id: String,
        request_id: String,
    },
    Close {
        view_id: String,
    },
    Run {
        view_id: String,
        request_id: String,
        revision: String,
        command: ChainEditorCommand,
    },
}

#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GatewayChainEditorOutcome {
    Ready {
        snapshot: ChainEditorSnapshot,
    },
    /// An unsaved Test result. It never changes the view's selected chain.
    Probed {
        probe: NativeUsdProbe,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone)]
pub struct GatewayChainEditorRequest {
    pub(super) session: u64,
    view_id: String,
    request_id: String,
    revision: String,
    command: ChainEditorCommand,
    deadline: Instant,
    live: Arc<AtomicBool>,
    view_live: Arc<AtomicBool>,
    authority: tokio::sync::watch::Receiver<GatewayWalletState>,
    wallet: Arc<GatewayWalletState>,
}
impl GatewayChainEditorRequest {
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.is_current_authority(&self.authority.borrow())
    }
    pub(super) fn is_current_authority(&self, authority: &GatewayWalletState) -> bool {
        Instant::now() < self.deadline
            && self.live.load(Ordering::Acquire)
            && self.view_live.load(Ordering::Acquire)
            && authority.view.is_some()
            && authority.same_authority(&self.wallet)
    }
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
    #[must_use]
    pub const fn command(&self) -> &ChainEditorCommand {
        &self.command
    }
}

struct Pending {
    id: String,
    live: Arc<AtomicBool>,
    completed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}
struct EditorView {
    live: Arc<AtomicBool>,
    selected: Option<String>,
    requests: HashSet<String>,
    pending: Option<Pending>,
}
impl Drop for EditorView {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}
#[derive(Default)]
pub(super) struct ChainEditorViews {
    views: HashMap<(u64, String), EditorView>,
    last_view: HashMap<u64, u64>,
}
impl ChainEditorViews {
    pub(super) fn retire_sessions(&mut self, live: impl Fn(u64) -> bool) {
        self.views.retain(|(session, _), _| live(*session));
        self.last_view.retain(|session, _| live(*session));
    }
    pub(super) fn retire_context(&mut self) {
        self.views.clear();
    }
}

impl DappProvider {
    pub(in crate::gateway) fn chain_editor_command(
        &mut self,
        session: u64,
        peer: PeerId,
        generation: u64,
        command: GatewayChainEditorCommand,
    ) -> Option<GatewayChainEditorRequest> {
        if generation != self.generation
            || self.ui_peers.get(&session) != Some(&peer)
            || self.wallet.view.is_none()
            || !self.authority.borrow().same_authority(&self.wallet)
        {
            return None;
        }
        let (view_id, request_id, revision, command) = match command {
            GatewayChainEditorCommand::Close { view_id } => {
                self.chain_editor_views.views.remove(&(session, view_id));
                return None;
            }
            GatewayChainEditorCommand::Open {
                view_id,
                request_id,
            } => {
                let sequence: u64 = view_id.parse().ok()?;
                let last = self
                    .chain_editor_views
                    .last_view
                    .entry(session)
                    .or_default();
                if sequence <= *last
                    || view_id != sequence.to_string()
                    || !valid_id(&request_id)
                    || self
                        .chain_editor_views
                        .views
                        .keys()
                        .filter(|(owner, _)| *owner == session)
                        .count()
                        >= 16
                {
                    return None;
                }
                // Keep the high-water mark after close and context retirement so old
                // commands cannot reopen a view. Memory is constant per live session.
                *last = sequence;
                self.chain_editor_views.views.insert(
                    (session, view_id.clone()),
                    EditorView {
                        live: Arc::new(AtomicBool::new(true)),
                        selected: None,
                        requests: HashSet::new(),
                        pending: None,
                    },
                );
                (view_id, request_id, String::new(), ChainEditorCommand::List)
            }
            GatewayChainEditorCommand::Run {
                view_id,
                request_id,
                revision,
                command,
            } => (view_id, request_id, revision, command),
        };
        let view = self
            .chain_editor_views
            .views
            .get_mut(&(session, view_id.clone()))?;
        if !valid_id(&request_id)
            || revision.len() > 128
            || view.requests.len() >= 256
            || view.requests.contains(&request_id)
            || view
                .pending
                .as_ref()
                .is_some_and(|pending| !pending.completed)
            || serde_json::to_vec(&command).ok()?.len() > crate::settings::MAX_CHAIN_MUTATION_BYTES
        {
            return None;
        }
        // The chain ID of an existing editor cannot be rebound by an untrusted frontend.
        let identity_matches = match &command {
            ChainEditorCommand::Save {
                draft,
                existing: true,
            } => view.selected.as_ref() == Some(&draft.chain_id),
            ChainEditorCommand::Save {
                existing: false, ..
            } => view.selected.is_none(),
            ChainEditorCommand::Remove { chain_id } | ChainEditorCommand::Reset { chain_id } => {
                view.selected.as_ref() == Some(chain_id)
            }
            // A Test reads the draft's own chain, which is the selected one unless it is new.
            ChainEditorCommand::Probe { draft } => view
                .selected
                .as_ref()
                .is_none_or(|selected| *selected == draft.chain_id),
            ChainEditorCommand::List | ChainEditorCommand::Inspect { .. } => true,
        };
        if !identity_matches {
            return None;
        }
        view.requests.insert(request_id.clone());
        let live = Arc::new(AtomicBool::new(true));
        view.pending = Some(Pending {
            id: request_id.clone(),
            live: live.clone(),
            completed: false,
        });
        Some(GatewayChainEditorRequest {
            session,
            view_id,
            request_id,
            revision,
            command,
            deadline: Instant::now() + Duration::from_mins(2),
            live,
            view_live: view.live.clone(),
            authority: self.authority.clone(),
            wallet: Arc::new(self.wallet.clone()),
        })
    }

    pub(in crate::gateway) fn complete_chain_editor_request(
        &mut self,
        request: GatewayChainEditorRequest,
        outcome: GatewayChainEditorOutcome,
    ) {
        if !request.is_current() {
            return;
        }
        let Some(view) = self
            .chain_editor_views
            .views
            .get_mut(&(request.session, request.view_id.clone()))
        else {
            return;
        };
        let Some(pending) = view.pending.as_mut().filter(|pending| {
            !pending.completed
                && pending.id == request.request_id
                && Arc::ptr_eq(&pending.live, &request.live)
        }) else {
            return;
        };
        pending.completed = true;
        if let GatewayChainEditorOutcome::Ready { snapshot } = &outcome {
            view.selected = snapshot.draft.as_ref().map(|draft| draft.chain_id.clone());
        }
        let mut delivery = Delivery::control(GatewayServerMessage::ChainEditor {
            version: 1,
            generation: self.generation,
            view_id: request.view_id.clone(),
            request_id: request.request_id.clone(),
            outcome,
        });
        let session = request.session;
        delivery.chain_editor = Some(request);
        self.outbox.push((session, delivery));
    }
}

#[cfg(test)]
// provider.rs mounts this file with #[path], so a plain `mod tests;` would resolve to gateway/tests.rs
#[path = "chain_editor/tests.rs"]
mod tests;
