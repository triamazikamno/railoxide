use super::{Context, WalletRoot, Window, WindowExt};
use crate::root::settings::SettingsApplyMode;
use railgun_ui::chain_editor::{ChainEditorCommand, ChainEditorSnapshot, NativeUsdProbe};
use wallet_ops::settings::{
    chain_editor_mutation, chain_editor_probe_chain, chain_editor_snapshot, editor_chain_id,
};

impl WalletRoot {
    pub(in crate::root) const fn gateway_chain_publication_available(&self) -> bool {
        self.gateway.handle.is_some() && self.gateway.desktop_state.is_some()
    }

    pub(in crate::root) fn apply_gateway_chain_editor(
        &mut self,
        request: wallet_ops::gateway::GatewayChainEditorRequest,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        use wallet_ops::gateway::GatewayChainEditorOutcome;
        if !request.is_current() || self.view_session.is_none() || *self.root_shutdown.borrow() {
            return;
        }
        let Some(handle) = self.gateway.handle.clone() else {
            return;
        };
        if matches!(request.command(), ChainEditorCommand::Probe { .. }) {
            self.probe_gateway_chain_editor(request, handle, cx);
            return;
        }
        let result = (|| -> Result<ChainEditorSnapshot, String> {
            let editor = self
                .settings_editor
                .clone()
                .ok_or("Settings are unavailable")?;
            let saved = editor.read(cx).saved.clone();
            let selected = match request.command() {
                ChainEditorCommand::List => return chain_editor_snapshot(&saved, None),
                ChainEditorCommand::Inspect { chain_id } => {
                    return chain_editor_snapshot(&saved, Some(editor_chain_id(chain_id)?));
                }
                ChainEditorCommand::Save { .. } | ChainEditorCommand::Remove { .. } => None,
                ChainEditorCommand::Reset { chain_id } => Some(editor_chain_id(chain_id)?),
                // Tests never reach this path; they are answered asynchronously above.
                ChainEditorCommand::Probe { .. } => {
                    return Err("This command does not change a chain".to_owned());
                }
            };
            let revision = request
                .revision()
                .parse()
                .map_err(|_| "Settings revision is invalid")?;
            let mutation = chain_editor_mutation(&saved, request.command())?;
            let candidate = mutation
                .prepare(&saved)
                .map_err(|error| error.to_string())?;
            self.admit_chain_settings(&candidate)?;
            if !request.is_current() {
                return Err("This editor is no longer available".to_owned());
            }
            let mode = editor.update(cx, |editor, cx| {
                editor.commit_chain_candidate(revision, candidate.clone(), cx)
            })?;
            if mode == SettingsApplyMode::NewRequests {
                self.apply_saved_request_settings(&candidate, cx);
            }
            let mut snapshot = chain_editor_snapshot(&candidate, selected)?;
            snapshot.restart_required = mode == SettingsApplyMode::NetworkingRestart;
            Ok(snapshot)
        })();
        let restart = result
            .as_ref()
            .is_ok_and(|snapshot| snapshot.restart_required);
        let outcome = match result {
            Ok(snapshot) => GatewayChainEditorOutcome::Ready { snapshot },
            Err(message) => GatewayChainEditorOutcome::Failed { message },
        };
        let completion = self.runtime.spawn(async move {
            handle.complete_chain_editor_request(request, outcome).await;
        });
        if restart {
            cx.spawn_in(window, async move |this, cx| {
                if completion.await.is_err() {
                    return;
                }
                let _ = this.update_in(cx, |root, window, cx| {
                    let startup = root
                        .settings_editor
                        .as_ref()
                        .and_then(|editor| editor.read(cx).startup_root.clone());
                    let http = root.reusable_network_context();
                    if let Some(startup) = startup {
                        window.close_all_dialogs(cx);
                        let _ = startup.update(cx, |startup, cx| {
                            startup.retry_startup_with_network_context(Some(http), window, cx);
                        });
                    }
                });
            })
            .detach();
        }
    }

    /// Reads a draft's own source for the extension's Test action. Nothing is persisted,
    /// cached or published, and a retired view receives no result.
    fn probe_gateway_chain_editor(
        &self,
        request: wallet_ops::gateway::GatewayChainEditorRequest,
        handle: wallet_ops::gateway::GatewayHandle,
        cx: &Context<'_, Self>,
    ) {
        use wallet_ops::gateway::GatewayChainEditorOutcome;
        let ChainEditorCommand::Probe { draft } = request.command() else {
            return;
        };
        let prepared = self
            .settings_editor
            .as_ref()
            .ok_or_else(|| "Settings are unavailable".to_owned())
            .and_then(|editor| chain_editor_probe_chain(&editor.read(cx).saved, draft));
        let http = self.reusable_network_context();
        self.runtime.spawn(async move {
            let result = match prepared {
                Ok(chain) => wallet_ops::probe_native_usd_quote(&chain, &http).await,
                Err(message) => Err(message),
            };
            if !request.is_current() {
                return;
            }
            handle
                .complete_chain_editor_request(
                    request,
                    GatewayChainEditorOutcome::Probed {
                        probe: NativeUsdProbe::new(result),
                    },
                )
                .await;
        });
    }
}
