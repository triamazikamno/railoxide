use crate::root::ui_helpers::dialog_footer;

use super::*;
use ui::controls::app_muted_shimmer_text;
use ui::dialog::WalletDialogExt as _;

mod fields;
mod maintenance;
mod tokens;
mod url_lists;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

const WALLET_DELETION_APPLY_STATUS: &str =
    "Wait for wallet deletion to finish before applying settings";

impl WalletSettingsEditor {
    pub(in crate::root) fn new(
        vault_store: Arc<DesktopVaultStore>,
        db_path: &Path,
        runtime: Handle,
        settings: WalletSettings,
        maintenance_controller: Entity<WalletMaintenanceController>,
        startup_root: Option<WeakEntity<WalletStartupRoot>>,
        active_root: Option<WeakEntity<WalletRoot>>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let maintenance_busy = !maintenance_controller.read(cx).is_idle();
        cx.observe(&maintenance_controller, |editor, controller, cx| {
            let busy = !controller.read(cx).is_idle();
            // A finished reset may have changed the folder's contents.
            if editor.maintenance_busy && !busy {
                editor.invalidate_data_folder_size();
            }
            editor.maintenance_busy = busy;
            cx.notify();
        })
        .detach();
        if let Some(root) = active_root.as_ref().and_then(WeakEntity::upgrade) {
            cx.observe(&root, |_editor, _root, cx| cx.notify()).detach();
        }
        let snapshot =
            wallet_ops::settings::chain_editor_snapshot(&settings, None).unwrap_or_default();
        let chain_editor = cx.new(|cx| {
            let mut editor = ui::chain_editor::ChainEditor::new(snapshot, cx);
            editor.set_staging(true, cx);
            editor
        });
        // The Chains page hands its body to the editor while a chain is open.
        cx.observe(&chain_editor, |editor, chain_editor, cx| {
            // Settings unmounts while a chain is open, which drops its selected page.
            let open = chain_editor.read(cx).is_editing();
            if editor.chain_editor_was_open && !open {
                editor.open_page = Some(CHAINS_PAGE_INDEX);
            }
            editor.chain_editor_was_open = open;
            cx.notify();
        })
        .detach();
        Self::subscribe_chain_editor(&chain_editor, true, window, cx);
        let mut editor = Self {
            chain_editor,
            vault_store,
            runtime,
            saved: settings.clone(),
            draft_base: settings.clone(),
            draft: settings,
            chain_editor_was_open: false,
            open_page: None,
            pre_start: false,
            poi_custom_publisher: false,
            field_sync_revision: 0,
            validation_error: None,
            status: None,
            cache_building: false,
            cache_build_progress: None,
            data_folder: data_folder_path(db_path),
            data_folder_size: DataFolderSize::Unknown,
            data_folder_size_generation: 0,
            data_folder_open_error: None,
            maintenance_busy,
            maintenance_controller,
            startup_root,
            active_root,
        };
        editor.refresh_validation();
        editor
    }

    pub(in crate::root) fn refresh_validation(&mut self) {
        self.validation_error = self
            .draft
            .validate()
            .err()
            .map(|error| Arc::from(error.to_string()));
    }

    pub(in crate::root) fn draft_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.status = None;
        self.refresh_validation();
        self.refresh_chain_editor(cx);
        cx.notify();
    }

    pub(super) const fn sync_fields_from_draft(&mut self) {
        self.field_sync_revision = self.field_sync_revision.wrapping_add(1);
    }

    pub(in crate::root) fn programmatic_draft_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.sync_fields_from_draft();
        self.draft_changed(cx);
    }

    /// Marks an editor owned by first-start setup, before any network context exists.
    pub(in crate::root) fn set_pre_start(&mut self, cx: &mut Context<'_, Self>) {
        self.pre_start = true;
        self.chain_editor.update(cx, |editor, cx| {
            editor.set_probe_disabled_reason(Some("Test is available after setup.".into()), cx);
        });
        cx.notify();
    }

    /// Selects `page_ix` the next time Settings mounts.
    pub(in crate::root) const fn open_on_page(&mut self, page_ix: usize) {
        self.open_page = Some(page_ix);
    }

    pub(in crate::root) fn set_draft_network_mode(
        &mut self,
        mode: NetworkModeSetting,
        cx: &mut Context<'_, Self>,
    ) {
        self.draft.network.mode = mode;
        if !should_show_proxy_url_setting(mode) {
            self.draft.network.proxy_url = None;
        }
        self.programmatic_draft_changed(cx);
    }

    /// Typed input owns its text, so this leaves the other fields unsynced like a typed field.
    pub(in crate::root) fn set_draft_proxy_url(&mut self, value: &str, cx: &mut Context<'_, Self>) {
        self.draft.network.proxy_url = non_empty_setting(value);
        self.draft_changed(cx);
    }

    pub(in crate::root) fn set_draft_chain_enabled(
        &mut self,
        chain_id: u64,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.draft
            .chains
            .per_chain
            .entry(chain_id)
            .or_insert_with(|| wallet_ops::settings::ChainSettingsOverride::for_chain(chain_id))
            .enabled = enabled;
        self.programmatic_draft_changed(cx);
    }

    pub(in crate::root) fn is_dirty(&self) -> bool {
        self.draft != self.draft_base
    }

    pub(in crate::root) fn root_replacement_is_allowed(&self, cx: &App) -> bool {
        if let Some(startup_root) = self.startup_root.as_ref() {
            return startup_root
                .read_with(cx, WalletStartupRoot::root_replacement_is_allowed)
                .unwrap_or(false);
        }
        self.active_root.as_ref().is_none_or(|root| {
            root.read_with(cx, |root, _cx| root.root_replacement_is_allowed())
                .unwrap_or(false)
        })
    }

    pub(in crate::root) fn render_status_indicator(&self, cx: &App) -> gpui::Div {
        let (label, color) = if !self.maintenance_controller.read(cx).is_idle() {
            ("Maintenance", theme::INFO)
        } else if !self.root_replacement_is_allowed(cx) {
            ("Wallet deletion", theme::INFO)
        } else if self.validation_error.is_some() {
            ("Invalid", theme::DANGER)
        } else if !self.is_dirty() {
            ("Saved", theme::SUCCESS)
        } else {
            match classify_settings_apply_mode(&self.saved, &self.draft) {
                SettingsApplyMode::NetworkingRestart if !self.pre_start => {
                    ("Restart required", theme::WARNING)
                }
                SettingsApplyMode::NetworkingRestart
                | SettingsApplyMode::NewRequests
                | SettingsApplyMode::FutureSessions => ("Unsaved", theme::WARNING),
                SettingsApplyMode::Clean => ("Saved", theme::SUCCESS),
            }
        };

        div()
            .w_full()
            .flex()
            .justify_end()
            .items_center()
            .gap_2()
            .text_size(px(12.0))
            .text_color(rgb(theme::TEXT_MUTED))
            .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
            .child(label)
    }

    pub(in crate::root) fn render_status_message(&self, cx: &App) -> Option<gpui::Div> {
        let controller = self.maintenance_controller.read(cx);
        let maintenance_status = controller.status();
        if controller.is_idle() && !self.root_replacement_is_allowed(cx) {
            return Some(settings_info_banner(WALLET_DELETION_APPLY_STATUS));
        }
        let status = if controller.is_idle() {
            self.status.as_ref().or(maintenance_status.as_ref())?
        } else {
            maintenance_status.as_ref().or(self.status.as_ref())?
        };
        if status.as_ref() == "Settings saved" {
            return None;
        }
        Some(settings_info_banner(status.to_string()))
    }

    pub(in crate::root) fn clear_transient_status(&mut self, cx: &mut Context<'_, Self>) {
        if self.cache_building || !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        self.maintenance_controller.update(cx, |controller, cx| {
            controller.clear_status(cx);
        });
        if self.status.take().is_some() {
            cx.notify();
        }
    }

    pub(in crate::root) fn add_dapp_token(
        &mut self,
        token: wallet_ops::settings::CustomTokenSettings,
        control: &wallet_ops::dapp_request::DappRequestControl,
        cx: &mut Context<'_, Self>,
    ) -> Result<wallet_ops::settings::EffectiveTokenRegistry, String> {
        if self.is_dirty() {
            return Err("Save or discard your settings changes before adding a token.".to_owned());
        }
        if !self.maintenance_controller.read(cx).is_idle() {
            return Err("Wait for maintenance to finish before adding a token.".to_owned());
        }
        let address = token
            .token_address
            .parse()
            .map_err(|_| "Token metadata is unavailable.".to_owned())?;
        let existing = wallet_ops::settings::build_effective_token_registry(&self.saved)
            .map_err(|_| "Saved token settings are unavailable.".to_owned())?;
        if existing.get(token.chain_id, &address).is_some() {
            return Ok(existing);
        }
        let mut settings = self.saved.clone();
        settings.tokens.custom_tokens.push(token);
        let registry = wallet_ops::settings::build_effective_token_registry(&settings)
            .map_err(|_| "Token metadata could not be added to settings.".to_owned())?;
        control
            .ensure_current()
            .map_err(|_| "This request is no longer available.".to_owned())?;
        wallet_ops::settings::settings_revision(&self.saved)
            .and_then(|revision| {
                wallet_ops::settings::commit_wallet_settings(
                    self.vault_store.db().as_ref(),
                    revision,
                    &settings,
                )
            })
            .map_err(|_| "Token settings could not be saved.".to_owned())?;
        self.saved = settings.clone();
        self.draft_base = settings.clone();
        self.draft = settings;
        self.sync_fields_from_draft();
        self.refresh_validation();
        self.refresh_chain_editor(cx);
        cx.notify();
        Ok(registry)
    }

    pub(in crate::root) fn save_draft(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return false;
        }
        if !self.pre_start
            && classify_settings_apply_mode(&self.saved, &self.draft)
                == SettingsApplyMode::NetworkingRestart
        {
            self.status = Some(Arc::from(
                "Use Apply and restart networking for networking changes",
            ));
            cx.notify();
            return false;
        }
        self.persist_draft(cx)
    }

    pub(in crate::root) fn persist_draft(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return false;
        }
        self.refresh_validation();
        if self.validation_error.is_some() {
            self.status = Some(Arc::from("Fix validation errors before saving settings"));
            cx.notify();
            return false;
        }
        let apply_mode = classify_settings_apply_mode(&self.saved, &self.draft);
        if (self.saved.chains != self.draft.chains || self.saved.tron != self.draft.tron)
            && let Some(root) = self.active_root.as_ref()
        {
            let admission = root
                .read_with(cx, |root, _cx| root.admit_chain_settings(&self.draft))
                .unwrap_or_else(|_| Err("The active wallet is unavailable".to_owned()));
            if let Err(message) = admission {
                self.status = Some(Arc::from(message));
                cx.notify();
                return false;
            }
        }
        let db = self.vault_store.db();
        match wallet_ops::settings::settings_revision(&self.draft_base).and_then(|revision| {
            wallet_ops::settings::commit_wallet_settings(db.as_ref(), revision, &self.draft)
        }) {
            Ok(()) => {
                self.saved = self.draft.clone();
                self.draft_base = self.saved.clone();
                self.apply_saved_settings_to_active_root(apply_mode, cx);
                self.status = Some(Arc::from("Settings saved"));
                cx.notify();
                true
            }
            Err(error) => {
                self.status = Some(Arc::from(format!("Failed to save settings: {error}")));
                cx.notify();
                false
            }
        }
    }

    pub(in crate::root) fn discard_changes(&mut self, cx: &mut Context<'_, Self>) {
        self.draft = settings_draft_after_discard(&self.saved);
        self.draft_base = self.saved.clone();
        self.poi_custom_publisher = false;
        self.sync_fields_from_draft();
        self.refresh_validation();
        self.refresh_chain_editor(cx);
        cx.notify();
    }

    pub(in crate::root) fn reset_defaults(&mut self, cx: &mut Context<'_, Self>) {
        self.draft = WalletSettings::default();
        self.poi_custom_publisher = false;
        self.sync_fields_from_draft();
        self.refresh_validation();
        self.refresh_chain_editor(cx);
        cx.notify();
    }

    /// The PPOI publisher choice reads Custom while selected or while the draft's publisher
    /// differs from the official preset.
    pub(in crate::root) fn poi_custom_publisher_selected(&self) -> bool {
        let official = wallet_ops::settings::PoiArtifactSettings::official_preset();
        let artifact = &self.draft.poi.artifact;
        self.poi_custom_publisher
            || artifact.publisher_pubkey != official.publisher_pubkey
            || artifact.manifest_source != official.manifest_source
    }

    /// Official restores only the publisher identity; gateways and manifest age stay as drafted.
    pub(in crate::root) fn select_poi_publisher(
        &mut self,
        custom: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.poi_custom_publisher = custom;
        if custom {
            cx.notify();
            return;
        }
        let official = wallet_ops::settings::PoiArtifactSettings::official_preset();
        self.draft.poi.artifact.publisher_pubkey = official.publisher_pubkey;
        self.draft.poi.artifact.manifest_source = official.manifest_source;
        self.programmatic_draft_changed(cx);
    }

    pub(in crate::root) fn apply_and_restart(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return;
        }
        if !self.root_replacement_is_allowed(cx) {
            self.status = Some(Arc::from(WALLET_DELETION_APPLY_STATUS));
            cx.notify();
            return;
        }
        let reusable_http = if settings_restart_reuses_active_network(&self.saved, &self.draft) {
            self.active_root.as_ref().and_then(|root| {
                root.update(cx, |root, _cx| root.reusable_network_context())
                    .ok()
            })
        } else {
            None
        };
        if !self.persist_draft(cx) {
            return;
        }
        window.close_all_dialogs(cx);
        if let Some(root) = self.startup_root.clone() {
            let _ = root.update(cx, |root, cx| {
                root.retry_startup_with_network_context(reusable_http, window, cx);
            });
        }
    }

    pub(in crate::root) fn apply_saved_settings_to_active_root(
        &self,
        apply_mode: SettingsApplyMode,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(root) = self.active_root.clone() else {
            return;
        };
        let settings = self.saved.clone();
        // Request settings may consult the editor through WalletConnect; release this update first.
        cx.defer(move |cx| {
            let _ = root.update(cx, |root, cx| {
                root.apply_saved_auto_lock_policy(&settings);
                root.apply_saved_tron_enabled(&settings, cx);
                if apply_mode == SettingsApplyMode::NewRequests {
                    root.apply_saved_request_settings(&settings, cx);
                }
            });
        });
    }
}
