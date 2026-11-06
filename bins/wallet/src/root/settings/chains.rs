use super::{
    Context, SettingsApplyMode, WalletSettings, WalletSettingsEditor, Window, WindowExt,
    classify_settings_apply_mode,
};
use gpui::{AppContext as _, Entity, SharedString};
use railgun_ui::chain_editor::{
    ChainDraft, ChainEditorCommand, ChainEditorSnapshot, ChainField, NativeUsdProbe,
};
use ui::chain_editor::{ChainEditor, ChainEditorEvent};
use ui::dialog::WalletDialogExt as _;
use wallet_ops::settings::{
    SettingsRevision, chain_editor_mutation, chain_editor_probe_chain, chain_editor_snapshot,
    commit_wallet_settings, editor_chain_id, settings_revision,
};

impl super::WalletRoot {
    pub(in crate::root) fn open_chain_editor(
        &self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use gpui::{
            InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
            Styled as _, div,
        };
        let Some(settings) = self.settings_editor.clone() else {
            return;
        };
        let mut refreshes = self.public_broadcaster_anchor_cache.subscribe_refreshes();
        // Not the Settings page editor, which stages; this dialog commits each chain at once.
        let (editor, snapshot) = settings.update(cx, |settings, cx| {
            let editor = cx.new(|cx| ChainEditor::new(ChainEditorSnapshot::default(), cx));
            WalletSettingsEditor::subscribe_chain_editor(&editor, false, window, cx);
            (
                editor,
                chain_editor_snapshot(&settings.saved, Some(chain_id)),
            )
        });
        let dialog_editor = editor.clone();
        let width = (window.viewport_size().width * 0.92).min(window.rem_size() * 40.0);
        let height = (window.viewport_size().height * 0.65).min(window.rem_size() * 28.0);
        window.open_wallet_dialog(cx, move |dialog, _, _| {
            dialog.title("Chain settings").w(width).child(
                // The list renders at its natural height, so the dialog body scrolls it.
                div()
                    .id("chain-settings-dialog-body")
                    .h(height)
                    .overflow_y_scroll()
                    .child(dialog_editor.clone()),
            )
        });
        editor.update(cx, |editor, cx| {
            editor.set_pricing_status(
                self.public_broadcaster_anchor_cache.native_usd_statuses(),
                cx,
            );
            editor.receive(snapshot, window, cx);
        });
        // The dialog has its own editor, so it follows pricing refreshes until it closes.
        let editor = editor.downgrade();
        cx.spawn(async move |root, cx| {
            while refreshes.changed().await.is_ok() {
                let Ok(statuses) = root.read_with(cx, |root, _| {
                    root.public_broadcaster_anchor_cache.native_usd_statuses()
                }) else {
                    break;
                };
                if editor
                    .update(cx, |editor, cx| editor.set_pricing_status(statuses, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

/// Search keywords for the Chains page: the name and ID of every listed chain, including draft-only
/// custom chains, and the chain editor's setting labels.
pub(in crate::root) fn chains_page_keywords(settings: &WalletSettings) -> Vec<SharedString> {
    let mut keywords: Vec<SharedString> = [
        "chains",
        "evm",
        "RPC endpoint",
        "Enable quick-sync",
        "Use preset sponsored relays",
        "Bypass Tor for the archive endpoint",
        "Native USD source",
    ]
    .into_iter()
    .chain(ChainField::ALL.iter().copied().map(ChainField::label))
    .map(SharedString::from)
    .collect();
    for chain_id in railgun_ui::built_in_chain_ids() {
        keywords.extend(railgun_ui::chain_name(chain_id).map(SharedString::from));
        keywords.push(chain_id.to_string().into());
    }
    for (chain_id, chain) in &settings.chains.custom {
        keywords.push(chain.name.clone().into());
        keywords.push(chain_id.to_string().into());
    }
    keywords
}

impl WalletSettingsEditor {
    /// Routes `view`'s commands here. A staging view edits the Settings draft; otherwise each
    /// confirmed chain is committed at once.
    pub(in crate::root) fn subscribe_chain_editor(
        view: &Entity<ChainEditor>,
        staging: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe_in(
            view,
            window,
            move |editor, view, event: &ChainEditorEvent, window, cx| {
                // A Test reads the network asynchronously and must not replace the draft.
                if let ChainEditorCommand::Probe { draft } = &event.command {
                    editor.handle_chain_editor_probe(draft, view, cx);
                    return;
                }
                let result = if staging {
                    editor.stage_chain_editor_command(&event.revision, &event.command, cx)
                } else {
                    editor.handle_chain_editor_command(&event.revision, &event.command, window, cx)
                };
                view.update(cx, |view, cx| view.receive(result, window, cx));
            },
        )
        .detach();
    }

    /// Shows the draft's chains in the Settings page editor, so staged edits stay listed and
    /// its revision matches the draft that staging checks against.
    pub(in crate::root) fn refresh_chain_editor(&self, cx: &mut Context<'_, Self>) {
        // A snapshot fails only when encoding fails; the list then keeps its last state.
        if let Ok(snapshot) = chain_editor_snapshot(&self.draft, None) {
            self.chain_editor
                .update(cx, |editor, cx| editor.refresh(snapshot, cx));
        }
    }

    /// Applies a Settings page chain command to the draft. The footer's Save or Apply commits it.
    pub(in crate::root) fn stage_chain_editor_command(
        &mut self,
        revision: &str,
        command: &ChainEditorCommand,
        cx: &mut Context<'_, Self>,
    ) -> Result<ChainEditorSnapshot, String> {
        let selected = match command {
            ChainEditorCommand::List => return chain_editor_snapshot(&self.draft, None),
            ChainEditorCommand::Inspect { chain_id } => {
                return chain_editor_snapshot(&self.draft, Some(editor_chain_id(chain_id)?));
            }
            ChainEditorCommand::Save { .. } | ChainEditorCommand::Remove { .. } => None,
            ChainEditorCommand::Reset { chain_id } => Some(editor_chain_id(chain_id)?),
            ChainEditorCommand::Probe { .. } => {
                return Err("This command does not change a chain".to_owned());
            }
        };
        let revision: SettingsRevision = revision
            .parse()
            .map_err(|_| "Settings revision is invalid")?;
        if settings_revision(&self.draft).map_err(|error| error.to_string())? != revision {
            return Err(wallet_ops::settings::WalletSettingsError::Conflict.to_string());
        }
        self.draft = chain_editor_mutation(&self.draft, command)?
            .prepare(&self.draft)
            .map_err(|error| error.to_string())?;
        self.programmatic_draft_changed(cx);
        chain_editor_snapshot(&self.draft, selected)
    }

    /// Called only after the active root's operation admission, on the GPUI thread.
    /// A dirty whole-settings draft deliberately retains its original revision.
    pub(in crate::root) fn commit_chain_candidate(
        &mut self,
        revision: SettingsRevision,
        candidate: WalletSettings,
        cx: &mut Context<'_, Self>,
    ) -> Result<SettingsApplyMode, String> {
        if !self.maintenance_controller.read(cx).is_idle() {
            return Err("Wait for maintenance before changing chains".to_owned());
        }
        if settings_revision(&self.saved).map_err(|error| error.to_string())? != revision {
            return Err(wallet_ops::settings::WalletSettingsError::Conflict.to_string());
        }
        let mode = classify_settings_apply_mode(&self.saved, &candidate);
        if mode == SettingsApplyMode::NetworkingRestart && self.is_dirty() {
            return Err(
                "Save or discard other Settings changes before restarting networking".to_owned(),
            );
        }
        commit_wallet_settings(self.vault_store.db().as_ref(), revision, &candidate)
            .map_err(|error| error.to_string())?;
        let dirty = self.is_dirty();
        self.saved = candidate;
        if !dirty {
            self.draft = self.saved.clone();
            self.draft_base = self.saved.clone();
            self.sync_fields_from_draft();
        }
        // Every commit path, including the extension's, refreshes the Settings list. A dirty
        // draft keeps its revision, so the commit surfaces as a conflict at Save or Apply.
        self.refresh_chain_editor(cx);
        self.refresh_validation();
        cx.notify();
        Ok(mode)
    }

    pub(in crate::root) fn handle_chain_editor_command(
        &mut self,
        revision: &str,
        command: &ChainEditorCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<ChainEditorSnapshot, String> {
        let selected = match command {
            ChainEditorCommand::List => return chain_editor_snapshot(&self.saved, None),
            ChainEditorCommand::Inspect { chain_id } => {
                return chain_editor_snapshot(&self.saved, Some(editor_chain_id(chain_id)?));
            }
            ChainEditorCommand::Save { .. } | ChainEditorCommand::Remove { .. } => None,
            ChainEditorCommand::Reset { chain_id } => Some(editor_chain_id(chain_id)?),
            // Tests are read-only and take the asynchronous path instead.
            ChainEditorCommand::Probe { .. } => {
                return Err("This command does not change a chain".to_owned());
            }
        };
        let revision = revision
            .parse()
            .map_err(|_| "Settings revision is invalid")?;
        let mutation = chain_editor_mutation(&self.saved, command)?;
        let candidate = mutation
            .prepare(&self.saved)
            .map_err(|error| error.to_string())?;
        if !self.root_replacement_is_allowed(cx) {
            return Err("Wait for wallet cleanup before changing chains".to_owned());
        }
        if let Some(root) = self.active_root.as_ref() {
            root.read_with(cx, |root, _| root.admit_chain_settings(&candidate))
                .map_err(|_| "The active wallet is unavailable")??;
        }
        let mode = self.commit_chain_candidate(revision, candidate, cx)?;
        self.apply_saved_settings_to_active_root(mode, cx);
        let mut snapshot = chain_editor_snapshot(&self.saved, selected)?;
        snapshot.restart_required = mode == SettingsApplyMode::NetworkingRestart;
        if snapshot.restart_required {
            let reusable_http = self.active_root.as_ref().and_then(|root| {
                root.read_with(cx, |root, _| root.reusable_network_context())
                    .ok()
            });
            window.close_all_dialogs(cx);
            if let Some(root) = self.startup_root.clone() {
                let _ = root.update(cx, |root, cx| {
                    root.retry_startup_with_network_context(reusable_http, window, cx);
                });
            }
        }
        Ok(snapshot)
    }

    /// Reads the draft's own source through the active wallet's network context. Nothing is
    /// persisted, cached or published, and the reply never replaces the open draft.
    pub(in crate::root) fn handle_chain_editor_probe(
        &self,
        draft: &ChainDraft,
        target: &Entity<ChainEditor>,
        cx: &mut Context<'_, Self>,
    ) {
        let prepared = chain_editor_probe_chain(&self.saved, draft).and_then(|chain| {
            let http = self
                .active_root
                .as_ref()
                .and_then(|root| {
                    root.read_with(cx, |root, _| root.reusable_network_context())
                        .ok()
                })
                .ok_or_else(|| "Wallet networking is not ready".to_owned())?;
            Ok::<_, String>((chain, http))
        });
        let (chain, http) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                target.update(cx, |editor, cx| {
                    editor.receive_probe(NativeUsdProbe::new(Err(message)), cx);
                });
                return;
            }
        };
        let join = self
            .runtime
            .spawn(async move { wallet_ops::probe_native_usd_quote(&chain, &http).await });
        let target = target.downgrade();
        cx.spawn(async move |_, cx| {
            let result = join
                .await
                .unwrap_or_else(|_| Err("The test could not be completed".to_owned()));
            let _ = target.update(cx, |editor, cx| {
                editor.receive_probe(NativeUsdProbe::new(result), cx);
            });
        })
        .detach();
    }
}
