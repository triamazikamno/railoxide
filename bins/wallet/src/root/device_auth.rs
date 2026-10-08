//! Touch ID and Apple Watch as alternatives to typing the vault password.
//!
//! Every prompt keeps its password field. Each enabled method gets an icon
//! inside the field that reads its sealed vault password and hands it to
//! the same submit path a typed password takes, so every vault check still runs
//! against the password itself.

use std::{
    cell::Cell,
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::ButtonVariants as _,
    dialog::Cancel,
    input::{InputEvent, InputGroupAddon, InputGroupAddonAlignment, InputGroupButton, InputState},
    notification::Notification,
};
use tokio::runtime::Handle;
use ui::controls::{
    app_button, app_input, app_input_group, app_masked_input, app_muted_text, app_strong_text,
};
use ui::theme;
pub(in crate::root) use wallet_ops::device_auth::DeviceAuthMethod;
use wallet_ops::device_auth::{DeviceAuthError, device_auth_available, device_auth_supported};
use wallet_ops::vault::{DesktopVaultStore, DeviceAuthStatus, VaultError};
use zeroize::Zeroizing;

use super::actions::DEVICE_AUTH_BUTTON_KEY_CONTEXT;
use super::{VaultState, WalletRoot, new_masked_input, secondary_dialog_content_width};
use crate::assets::RailgunActionIcon;

const ENABLE_DEVICE_AUTH_DIALOG_WIDTH: Pixels = px(420.0);

#[cfg(test)]
mod tests;

/// Each reason finishes the "… is trying to" sentence of the system Touch ID prompt.
pub(in crate::root) const DEVICE_AUTH_REASON_UNLOCK: &str = "unlock the wallet vault";
pub(in crate::root) const DEVICE_AUTH_REASON_SPEND: &str = "authorize this action";
pub(in crate::root) const DEVICE_AUTH_REASON_KEY_EXPORT: &str = "reveal wallet keys";
pub(in crate::root) const DEVICE_AUTH_REASON_CHANGE_PASSWORD: &str = "change the vault password";
pub(in crate::root) const DEVICE_AUTH_REASON_ADD_WALLET: &str = "add a wallet";
pub(in crate::root) const DEVICE_AUTH_REASON_PUBLIC_ACCOUNT: &str = "add a public account";
pub(in crate::root) const DEVICE_AUTH_REASON_PASSPHRASE_WALLET: &str = "open a passphrase wallet";

/// Reads the sealed vault password using one explicitly selected method.
#[derive(Clone)]
pub(in crate::root) struct DeviceAuthPrompt {
    runtime: Handle,
    store: Arc<DesktopVaultStore>,
    methods: Vec<DeviceAuthMethod>,
}

pub(in crate::root) enum DeviceAuthPassword {
    Password(Zeroizing<String>),
    /// The prompt was dismissed; leave the form as it was.
    Cancelled,
    Failed(Arc<str>),
}

impl DeviceAuthPrompt {
    pub(in crate::root) fn includes(&self, method: DeviceAuthMethod) -> bool {
        self.methods.contains(&method)
    }

    pub(in crate::root) fn without(mut self, method: DeviceAuthMethod) -> Option<Self> {
        self.methods.retain(|&other| other != method);
        (!self.methods.is_empty()).then_some(self)
    }

    /// Shows the system authentication prompt off the UI thread and passes the
    /// outcome to `on_done` on the view that asked for it.
    pub(in crate::root) fn run<V: 'static>(
        &self,
        method: DeviceAuthMethod,
        reason: &'static str,
        window: &Window,
        cx: &Context<'_, V>,
        on_done: impl FnOnce(&mut V, DeviceAuthPassword, &mut Window, &mut Context<'_, V>) + 'static,
    ) {
        let store = Arc::clone(&self.store);
        let enabled = self.methods.contains(&method);
        let join = self.runtime.spawn_blocking(move || {
            if !enabled {
                return Err(VaultError::DeviceAuthDisabled);
            }
            store.device_auth_vault_password(method, reason)
        });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = match join.await {
                Ok(Ok(password)) => DeviceAuthPassword::Password(password),
                Ok(Err(error)) => device_auth_failure(method, &error),
                Err(error) => {
                    tracing::warn!(%error, "device authentication task failed");
                    DeviceAuthPassword::Failed(Arc::from(format!(
                        "{} failed. Enter the vault password instead.",
                        method.label()
                    )))
                }
            };
            let _ = this.update_in(cx, |view, window, cx| on_done(view, outcome, window, cx));
        })
        .detach();
    }
}

fn device_auth_failure(method: DeviceAuthMethod, error: &VaultError) -> DeviceAuthPassword {
    match error {
        VaultError::DeviceAuth(DeviceAuthError::Cancelled) => DeviceAuthPassword::Cancelled,
        VaultError::DeviceAuth(DeviceAuthError::LockedOut | DeviceAuthError::InProgress) => {
            DeviceAuthPassword::Failed(Arc::from(error.to_string()))
        }
        VaultError::DeviceAuth(DeviceAuthError::Unavailable) => {
            DeviceAuthPassword::Failed(Arc::from(format!(
                "{} is not available right now. Enter the vault password instead.",
                method.label()
            )))
        }
        VaultError::DeviceAuthDisabled => DeviceAuthPassword::Failed(Arc::from(format!(
            "{} needs the vault password once. Enter it to continue.",
            method.label()
        ))),
        error => {
            tracing::warn!(%error, "device authentication failed");
            DeviceAuthPassword::Failed(Arc::from(format!(
                "{} failed. Enter the vault password instead.",
                method.label()
            )))
        }
    }
}

/// Explicit actions share one field and pending state. Each icon requests only
/// its named method; the caller retains the same password submission path.
pub(in crate::root) fn device_auth_buttons(
    prompt: Option<&DeviceAuthPrompt>,
    id: &'static str,
    pending: bool,
    disabled: bool,
    on_click: impl Fn(DeviceAuthMethod, &mut Window, &mut App) + 'static,
) -> Vec<InputGroupButton> {
    let Some(prompt) = prompt else {
        return Vec::new();
    };
    let on_click = Rc::new(on_click);
    prompt
        .methods
        .iter()
        .map(|&method| {
            let on_click = on_click.clone();
            let (id, icon, tooltip) = match method {
                DeviceAuthMethod::TouchId => (
                    id.to_owned(),
                    RailgunActionIcon::Fingerprint,
                    "Use Touch ID instead of the vault password",
                ),
                DeviceAuthMethod::AppleWatch => (
                    id.replace("touch-id", "apple-watch"),
                    RailgunActionIcon::Watch,
                    "Use Apple Watch. Double-press the side button to approve.",
                ),
            };
            InputGroupButton::new(id)
                .key_context(DEVICE_AUTH_BUTTON_KEY_CONTEXT)
                .icon(Icon::new(icon))
                .accessibility_label(format!("Use {}", method.label()))
                .tooltip(tooltip)
                .loading(pending)
                .disabled(disabled || pending)
                .on_click(move |_, window, cx| on_click(method, window, cx))
        })
        .collect()
}

pub(in crate::root) fn masked_input_with_device_auth(
    input: &Entity<InputState>,
    disabled: bool,
    buttons: Vec<InputGroupButton>,
) -> gpui::Div {
    if buttons.is_empty() {
        return app_masked_input(input, disabled);
    }
    div().w_full().child(
        app_input_group(
            ("vault-password-device-auth", input.entity_id()),
            input,
            "Vault password",
        )
        .input(
            app_input(input)
                .role(gpui::accesskit::Role::PasswordInput)
                .bg(gpui::transparent_black()),
        )
        .disabled(disabled)
        .addon(
            InputGroupAddon::new(("vault-password-device-auth-actions", input.entity_id()))
                .align(InputGroupAddonAlignment::InlineEnd)
                .children(buttons),
        ),
    )
}

impl WalletRoot {
    pub(in crate::root) fn refresh_device_auth_status(&mut self) {
        self.touch_id_supported = device_auth_supported(DeviceAuthMethod::TouchId);
        self.apple_watch_supported = device_auth_supported(DeviceAuthMethod::AppleWatch);
        self.touch_id_available = device_auth_available(DeviceAuthMethod::TouchId);
        self.apple_watch_available = device_auth_available(DeviceAuthMethod::AppleWatch);
        for method in DeviceAuthMethod::ALL {
            let status = self.vault_store.as_ref().and_then(|store| {
                store.device_auth_status(method)
                    .inspect_err(|error| tracing::warn!(%error, ?method, "failed to read device authentication status"))
                    .ok()
            }).unwrap_or(DeviceAuthStatus::Disabled);
            match method {
                DeviceAuthMethod::TouchId => self.touch_id_status = status,
                DeviceAuthMethod::AppleWatch => self.apple_watch_status = status,
            }
        }
    }

    pub(in crate::root) fn device_auth_prompt(&mut self) -> Option<DeviceAuthPrompt> {
        self.refresh_device_auth_status();
        self.device_auth_prompt_cached()
    }

    pub(in crate::root) fn device_auth_prompt_cached(&self) -> Option<DeviceAuthPrompt> {
        let methods = DeviceAuthMethod::ALL
            .into_iter()
            .filter(|&method| {
                self.device_auth_status(method) == DeviceAuthStatus::Enabled
                    && (cfg!(target_os = "macos") || self.device_auth_supported(method))
            })
            .collect::<Vec<_>>();
        if methods.is_empty() {
            return None;
        }
        Some(DeviceAuthPrompt {
            runtime: self.runtime.clone(),
            store: Arc::clone(self.vault_store.as_ref()?),
            methods,
        })
    }

    pub(in crate::root) const fn device_auth_status(
        &self,
        method: DeviceAuthMethod,
    ) -> DeviceAuthStatus {
        match method {
            DeviceAuthMethod::TouchId => self.touch_id_status,
            DeviceAuthMethod::AppleWatch => self.apple_watch_status,
        }
    }

    const fn device_auth_supported(&self, method: DeviceAuthMethod) -> bool {
        match method {
            DeviceAuthMethod::TouchId => self.touch_id_supported,
            DeviceAuthMethod::AppleWatch => self.apple_watch_supported,
        }
    }

    /// Snapshot for Settings presentation; enrollment rechecks with macOS.
    pub(in crate::root) const fn device_auth_available(&self, method: DeviceAuthMethod) -> bool {
        match method {
            DeviceAuthMethod::TouchId => self.touch_id_available,
            DeviceAuthMethod::AppleWatch => self.apple_watch_available,
        }
    }

    /// Saved enrollment stays visible when its device is temporarily unavailable.
    pub(in crate::root) const fn device_auth_setting_state(
        &self,
        method: DeviceAuthMethod,
    ) -> Option<bool> {
        match self.device_auth_status(method) {
            DeviceAuthStatus::Enabled | DeviceAuthStatus::NeedsReenrollment => Some(true),
            DeviceAuthStatus::Disabled if self.device_auth_supported(method) => Some(false),
            DeviceAuthStatus::Disabled => None,
        }
    }

    /// Reseals stale device enrollments with a password that was just verified.
    /// Runs on the caller's blocking thread.
    pub(in crate::root) fn renew_device_auth(store: &DesktopVaultStore, password: &str) {
        for method in DeviceAuthMethod::ALL {
            if let Err(error) = store.renew_device_auth(method, password) {
                tracing::warn!(%error, ?method, "failed to renew device authentication");
            }
        }
    }

    /// Offers Touch ID once when the app starts on the unlock screen. Locking
    /// later does not prompt, so the system dialog never waits on an idle Mac.
    pub(in crate::root) fn prompt_device_auth_on_unlock_screen_if_requested(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if std::mem::take(&mut self.touch_id_unlock_on_render)
            && matches!(self.vault_state, VaultState::UnlockVault)
        {
            cx.defer_in(window, |root, window, cx| {
                if matches!(root.vault_state, VaultState::UnlockVault) {
                    root.unlock_vault_with_device_auth(DeviceAuthMethod::TouchId, window, cx);
                }
            });
        }
    }

    pub(in crate::root) fn unlock_vault_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.unlock_in_progress || self.device_auth_in_progress {
            return;
        }
        let Some(prompt) = self.device_auth_prompt() else {
            cx.notify();
            return;
        };
        self.device_auth_in_progress = true;
        self.vault_error = None;
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_UNLOCK,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_vault_device_auth_unlock(generation, outcome, window, cx);
            },
        );
    }

    fn finish_vault_device_auth_unlock(
        &mut self,
        generation: u64,
        outcome: DeviceAuthPassword,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_in_progress = false;
        if matches!(&outcome, DeviceAuthPassword::Failed(_)) {
            self.refresh_device_auth_status();
        }
        cx.notify();
        if self.active_wallet_generation != generation
            || !matches!(self.vault_state, VaultState::UnlockVault)
        {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => {
                self.unlock_vault_with_password(password, None, window, cx);
            }
            DeviceAuthPassword::Cancelled => {
                self.focus_vault_input_on_render = true;
            }
            DeviceAuthPassword::Failed(message) => {
                self.focus_vault_input_on_render = true;
                self.vault_error = Some(message);
            }
        }
    }

    /// Authenticates the add-wallet password field. The user still
    /// picks the action, such as confirming a saved recovery phrase.
    pub(in crate::root) fn authorize_add_wallet_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth_in_progress
            || !self
                .add_wallet_dialog_lease
                .upgrade()
                .is_some_and(|open| open.get())
        {
            return;
        }
        let Some(prompt) = self.device_auth_prompt() else {
            cx.notify();
            return;
        };
        self.device_auth_in_progress = true;
        self.vault_error = None;
        let lease = self.add_wallet_dialog_lease.clone();
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_ADD_WALLET,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_add_wallet_device_auth(&lease, generation, outcome, window, cx);
            },
        );
    }

    fn finish_add_wallet_device_auth(
        &mut self,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_in_progress = false;
        if matches!(&outcome, DeviceAuthPassword::Failed(_)) {
            self.refresh_device_auth_status();
        }
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.active_wallet_generation != generation
            || !matches!(self.vault_state, VaultState::ViewUnlocked)
        {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => {
                self.add_wallet_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.add_wallet_device_auth_password = Some(password);
            }
            DeviceAuthPassword::Cancelled => {}
            DeviceAuthPassword::Failed(message) => {
                self.vault_error = Some(message);
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn unlock_hardware_profile_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth_in_progress
            || self.hardware_profile_unlock.in_progress
            || !self
                .hardware_profile_unlock
                .dialog_lease
                .upgrade()
                .is_some_and(|open| open.get())
        {
            return;
        }
        let Some(prompt) = self.device_auth_prompt() else {
            cx.notify();
            return;
        };
        self.device_auth_in_progress = true;
        self.hardware_profile_unlock.error = None;
        let generation = self.hardware_wallet_creation_generation;
        let lease = self.hardware_profile_unlock.dialog_lease.clone();
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_UNLOCK,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_hardware_profile_device_auth_unlock(
                    &lease, generation, outcome, window, cx,
                );
            },
        );
    }

    #[cfg(feature = "hardware")]
    fn finish_hardware_profile_device_auth_unlock(
        &mut self,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_in_progress = false;
        if matches!(&outcome, DeviceAuthPassword::Failed(_)) {
            self.refresh_device_auth_status();
        }
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.hardware_wallet_creation_generation != generation
        {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => {
                if self.hardware_profile_unlock_requires_password()
                    && !self.hardware_profile_unlock.in_progress
                {
                    self.hardware_profile_device_auth_password = Some(password);
                    self.unlock_hardware_profile_from_dialog(window, cx);
                    self.hardware_profile_device_auth_password = None;
                }
            }
            DeviceAuthPassword::Cancelled => {}
            DeviceAuthPassword::Failed(message) => {
                self.hardware_profile_unlock.error = Some(message);
            }
        }
    }

    pub(in crate::root) fn clear_add_wallet_password(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.add_wallet_device_auth_password = None;
        self.add_wallet_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// The add-wallet password field, or confirmation of device authentication.
    pub(in crate::root) fn render_add_wallet_password(
        &self,
        root: Entity<Self>,
        disabled: bool,
    ) -> gpui::Div {
        if self.add_wallet_device_auth_password.is_some() {
            return div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .px(px(10.0))
                .py(px(6.0))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    Icon::new(gpui_component::IconName::Check)
                        .size_4()
                        .text_color(rgb(theme::SUCCESS)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(app_muted_text("Vault password provided")),
                )
                .child(
                    app_button("add-wallet-touch-id-clear", "Type instead")
                        .ghost()
                        .xsmall()
                        .flex_none()
                        .disabled(disabled)
                        .on_click(move |_event, window, cx| {
                            root.update(cx, |root, cx| {
                                root.clear_add_wallet_password(window, cx);
                                cx.notify();
                            });
                        }),
                );
        }
        masked_input_with_device_auth(
            &self.add_wallet_password_input,
            disabled || self.device_auth_in_progress,
            device_auth_buttons(
                self.device_auth_prompt_cached().as_ref(),
                "add-wallet-touch-id",
                self.device_auth_in_progress,
                disabled,
                move |method, window, cx| {
                    root.update(cx, |root, cx| {
                        root.authorize_add_wallet_with_device_auth(method, window, cx);
                    });
                },
            ),
        )
    }

    pub(in crate::root) fn disable_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.as_ref() else {
            return;
        };
        match store.disable_device_auth(method) {
            Ok(()) => {
                window.push_notification(
                    Notification::success(format!("{} unlock turned off.", method.label())),
                    cx,
                );
            }
            Err(error) => {
                tracing::warn!(%error, "failed to turn off device authentication");
                window.push_notification(
                    Notification::error(format!("Failed to turn off {}: {error}", method.label())),
                    cx,
                );
            }
        }
        self.refresh_device_auth_status();
        cx.notify();
    }

    pub(in crate::root) fn open_enable_device_auth_dialog(
        &self,
        method: DeviceAuthMethod,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth_available(method) {
            EnableDeviceAuthDialogContent::open(method, window, cx);
        }
    }

    /// Seals the password that just created the vault when the user opted in.
    pub(in crate::root) fn enable_touch_id_after_vault_creation(
        &self,
        password: &Zeroizing<String>,
        cx: &Context<'_, Self>,
    ) {
        if !(self.touch_id_supported && self.enable_touch_id_on_create) {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let password = password.clone();
        let join = self.runtime.spawn_blocking(move || {
            store.enable_device_auth(
                wallet_ops::device_auth::DeviceAuthMethod::TouchId,
                password.as_str(),
            )
        });
        cx.spawn(async move |this, cx| {
            match join.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "failed to turn on Touch ID for the new vault");
                }
                Err(error) => tracing::warn!(%error, "turn on Touch ID task failed"),
            }
            let _ = this.update(cx, |root, cx| {
                root.refresh_device_auth_status();
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn set_enable_touch_id_on_create(
        &mut self,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.enable_touch_id_on_create = enabled;
        cx.notify();
    }
}

struct EnableDeviceAuthDialogContent {
    method: DeviceAuthMethod,
    root: Entity<WalletRoot>,
    password_input: Entity<InputState>,
    error: Option<Arc<str>>,
    pending: bool,
    lease: Weak<Cell<bool>>,
    dialog_focus: Option<FocusHandle>,
}

impl EnableDeviceAuthDialogContent {
    fn open(
        method: DeviceAuthMethod,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) -> Entity<Self> {
        let root = cx.entity();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let content = cx.new(|cx| Self::new(method, root, identity, window, cx));
        let dialog_content = content.clone();
        let dialog_width =
            (window.viewport_size().width * 0.92).min(ENABLE_DEVICE_AUTH_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let identity = Rc::downgrade(&lease);
            let cancel_content = dialog_content.clone();
            let pending = dialog_content.read(cx).pending;
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, cx| !cancel_content.read(cx).pending)
                .on_close(move |_, _, _| {
                    if let Some(lease) = identity.upgrade() {
                        lease.set(false);
                    }
                })
                .close_button(!pending)
                .overlay_closable(!pending)
                .title(app_strong_text(format!("Turn on {}", method.label())))
                .child(div().w(content_width).child(dialog_content.clone()))
        });
        content.update(cx, |content, cx| content.dialog_focus = window.focused(cx));
        let focus_content = content.clone();
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
        content
    }

    fn new(
        method: DeviceAuthMethod,
        root: Entity<WalletRoot>,
        lease: Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let password_input = new_masked_input(window, cx, "vault password");
        cx.subscribe_in(
            &password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        Self {
            method,
            root,
            password_input,
            error: None,
            pending: false,
            lease,
            dialog_focus: None,
        }
    }

    fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending {
            return;
        }
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if password.trim().is_empty() {
            self.error = Some(Arc::from(format!(
                "Enter the vault password to turn on {}",
                self.method.label()
            )));
            cx.notify();
            return;
        }
        let root = self.root.read(cx);
        let Some(store) = root.vault_store.clone() else {
            self.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let method = self.method;
        let join = root
            .runtime
            .spawn_blocking(move || store.enable_device_auth(method, password.as_str()));
        self.observe_enrollment(
            async move {
                match join.await {
                    Ok(result) => {
                        result.map_err(|error| enable_device_auth_error_message(method, &error))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "device authentication enrollment task failed");
                        Err(Arc::from(format!(
                            "Failed to turn on {}. Try again.",
                            method.label()
                        )))
                    }
                }
            },
            window,
            cx,
        );
    }

    fn observe_enrollment(
        &mut self,
        completion: impl Future<Output = Result<(), Arc<str>>> + 'static,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending = true;
        self.error = None;
        let root = self.root.downgrade();
        let method = self.method;
        let lease = self.lease.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = completion.await;
            // Enrollment commits in the worker even if locking forcibly closes its dialog.
            let _ = root.update(cx, |root, cx| {
                root.refresh_device_auth_status();
                cx.notify();
            });
            let original_open = lease.upgrade().is_some_and(|open| open.get());
            let _ = cx.update(|window, cx| match &result {
                Ok(()) => window.push_notification(
                    Notification::success(format!(
                        "{} unlock turned on. The vault password still works.",
                        method.label()
                    )),
                    cx,
                ),
                Err(error) if !original_open => {
                    window.push_notification(Notification::error(error.to_string()), cx);
                }
                Err(_) => {}
            });
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                cx.notify();
                if !original_open {
                    return;
                }
                let focused = dialog
                    .dialog_focus
                    .as_ref()
                    .is_some_and(|focus| focus.contains_focused(window, cx));
                match result {
                    Ok(()) => {
                        if focused {
                            window.close_dialog(cx);
                        }
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                        if focused {
                            dialog.focus_password(window, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }
}

fn enable_device_auth_error_message(method: DeviceAuthMethod, error: &VaultError) -> Arc<str> {
    match error {
        VaultError::UnlockFailed => {
            Arc::from("Password did not unlock the vault. Check it and try again.")
        }
        VaultError::DeviceAuth(DeviceAuthError::Unavailable) => Arc::from(format!(
            "{} is not available on this Mac right now.",
            method.label()
        )),
        VaultError::DeviceAuth(DeviceAuthError::LockedOut) => Arc::from(error.to_string()),
        error => {
            tracing::warn!(%error, "failed to turn on device authentication");
            Arc::from(format!("Failed to turn on {}: {error}", method.label()))
        }
    }
}

impl Render for EnableDeviceAuthDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let cancel_id = match self.method {
            DeviceAuthMethod::TouchId => "wallet-enable-touch-id-cancel",
            DeviceAuthMethod::AppleWatch => "wallet-enable-apple-watch-cancel",
        };
        let submit_id = match self.method {
            DeviceAuthMethod::TouchId => "wallet-enable-touch-id-submit",
            DeviceAuthMethod::AppleWatch => "wallet-enable-apple-watch-submit",
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                app_muted_text(
                    match self.method {
                        DeviceAuthMethod::TouchId => "Enter the vault password to use Touch ID wherever the wallet asks for it. The password stays sealed in this Mac's Secure Enclave and only opens with a fingerprint enrolled now. Typing the password keeps working.",
                        DeviceAuthMethod::AppleWatch => "Enter the vault password to use Apple Watch wherever the wallet asks for it. Each approval requires a double-press of the watch's side button. The vault never unlocks just because your watch is nearby. Typing the password keeps working.",
                    },
                )
                .whitespace_normal(),
            )
            .child(app_masked_input(&self.password_input, self.pending))
            .when_some(self.error.as_ref(), |this, error| {
                this.child(
                    app_muted_text(error.to_string())
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button(cancel_id, "Cancel")
                            .debug_selector(move || cancel_id.into())
                            .flex_none()
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                window.dispatch_action(Box::new(Cancel), cx);
                            }),
                    )
                    .child(
                        app_button(submit_id, "Turn on")
                            .primary()
                            .flex_none()
                            .loading(self.pending)
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
    }
}
