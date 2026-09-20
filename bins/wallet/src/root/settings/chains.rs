use super::{
    Context, SettingsApplyMode, WalletSettings, WalletSettingsEditor, Window, WindowExt,
    classify_settings_apply_mode,
};
use railgun_ui::chain_editor::{ChainEditorCommand, ChainEditorSnapshot};
use wallet_ops::settings::{
    SettingsRevision, chain_editor_mutation, chain_editor_snapshot, commit_wallet_settings,
    editor_chain_id, settings_revision,
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
        let Some(settings) = self.settings_editor.as_ref() else {
            return;
        };
        let settings = settings.read(cx);
        let snapshot = chain_editor_snapshot(&settings.saved, Some(chain_id));
        let editor = settings.chain_editor.clone();
        let dialog_editor = editor.clone();
        let width = (window.viewport_size().width * 0.92).min(window.rem_size() * 40.0);
        let height = (window.viewport_size().height * 0.65).min(window.rem_size() * 28.0);
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Chain settings").w(width).child(
                // The list renders at its natural height, so the dialog body scrolls it.
                div()
                    .id("chain-settings-dialog-body")
                    .h(height)
                    .overflow_y_scroll()
                    .child(dialog_editor.clone()),
            )
        });
        editor.update(cx, |editor, cx| editor.receive(snapshot, window, cx));
    }
}

impl WalletSettingsEditor {
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
        // Every commit path, including the extension's, refreshes the shared list.
        if let Ok(snapshot) = chain_editor_snapshot(&self.saved, None) {
            self.chain_editor
                .update(cx, |editor, cx| editor.refresh(snapshot, cx));
        }
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
}
