use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, img, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{ButtonGroup, ButtonVariants},
    checkbox::Checkbox,
    collapsible::Collapsible,
    input::{InputEvent, InputState},
};
use ui::controls::{app_button, app_input, app_masked_input, app_muted_text, app_segment_button};
use ui::{icons, theme};
use wallet_ops::vault::{SoftwareContextSyncIntent, WalletMetadataBundle};
use zeroize::Zeroizing;

use super::super::{
    WalletRoot, labeled_field, new_masked_input, new_text_input, secondary_dialog_content_width,
    vault_ui::vault_dialog_body,
};
use super::{PendingSoftwareProfileOpenStage, VaultState, passphrase_open_action_is_eligible};

pub(in crate::root) struct OpenPassphraseWalletAuthorizationUi {
    root: Entity<WalletRoot>,
    target_base_profile_uuid: Arc<str>,
    target_label: Arc<str>,
    password_input: Entity<InputState>,
    error: Option<Arc<str>>,
}

impl OpenPassphraseWalletAuthorizationUi {
    fn new(
        root: Entity<WalletRoot>,
        target_base_profile_uuid: Arc<str>,
        target_label: Arc<str>,
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
                    this.root.update(cx, |root, _cx| {
                        root.vault_error = None;
                    });
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        Self {
            root,
            target_base_profile_uuid,
            target_label,
            password_input,
            error: None,
        }
    }

    fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if password.trim().is_empty() {
            self.error = Some(Arc::from(
                "Enter the vault password to open a passphrase wallet.",
            ));
            cx.notify();
            return;
        }

        self.error = None;
        let target_base_profile_uuid = self.target_base_profile_uuid.clone();
        self.root.update(cx, |root, cx| {
            root.begin_open_passphrase_wallet(target_base_profile_uuid, password, window, cx);
        });
        cx.notify();
    }

    fn render_error(&self, cx: &Context<'_, Self>) -> Option<gpui::AnyElement> {
        self.error
            .clone()
            .or_else(|| self.root.read(cx).vault_error.clone())
            .map(|message| passphrase_error_alert("open-passphrase-wallet-error", &message))
    }
}

impl Render for OpenPassphraseWalletAuthorizationUi {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(img(icons::lock_icon_path()).size(px(40.0)).flex_none()),
            )
            .child(vault_dialog_body(format!(
                "Enter your vault password to continue. You'll enter the mnemonic passphrase for \"{}\" on the next screen.",
                self.target_label.as_ref()
            )))
            .child(app_masked_input(&self.password_input, false))
            .children(self.render_error(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("open-passphrase-wallet-cancel", "Cancel")
                            .on_click(move |_event, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        app_button("open-passphrase-wallet-submit", "Continue")
                            .primary()
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
    }
}

impl WalletRoot {
    pub(in crate::root) fn open_passphrase_wallet_authorization_dialog(
        &mut self,
        target_base_profile_uuid: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(eligible) = self
            .wallet_metadata
            .iter()
            .find(|wallet| wallet.wallet_uuid == target_base_profile_uuid.as_ref())
        else {
            return;
        };
        if !passphrase_open_action_is_eligible(Some(eligible)) {
            return;
        }
        let target_label: Arc<str> = Arc::from(eligible.label.clone());
        self.vault_error = None;
        let root = cx.entity();
        let content = cx.new(|cx| {
            OpenPassphraseWalletAuthorizationUi::new(
                root.clone(),
                target_base_profile_uuid,
                target_label,
                window,
                cx,
            )
        });
        let focus_content = content.clone();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(420.0));
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .child(div().w(content_width).child(content.clone()))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingOpenUiStage {
    Choosing,
    UnknownDecision,
    Creation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingOpenPrimaryRoute {
    Standard,
    Passphrase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingOpenActionAvailability {
    continue_without_passphrase: bool,
    submit_passphrase: bool,
    retry: bool,
    add_passphrase_wallet: bool,
    create_context: bool,
    cancel: bool,
}

pub(in crate::root) struct PassphraseOpenUi {
    root: Entity<WalletRoot>,
    passphrase_input: Entity<InputState>,
    confirmation_input: Entity<InputState>,
    label_input: Entity<InputState>,
    remember_standard_context: bool,
    intent: SoftwareContextSyncIntent,
    stage: PendingOpenUiStage,
    operation_active: bool,
    about_passphrases_open: bool,
    error: Option<Arc<str>>,
}

impl PassphraseOpenUi {
    pub(in crate::root) fn new(
        root: Entity<WalletRoot>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let passphrase_input = new_masked_input(window, cx, "mnemonic passphrase");
        let confirmation_input = new_masked_input(window, cx, "enter passphrase again");
        let label_input = new_text_input(window, cx, "wallet label");

        let passphrase_for_events = passphrase_input.clone();
        cx.subscribe_in(
            &passphrase_for_events,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit_primary(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();

        let confirmation_for_events = confirmation_input.clone();
        cx.subscribe_in(
            &confirmation_for_events,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit_creation(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();

        cx.subscribe(&label_input, |this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                cx.notify();
            }
        })
        .detach();

        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();

        Self {
            root,
            passphrase_input,
            confirmation_input,
            label_input,
            remember_standard_context: false,
            intent: SoftwareContextSyncIntent::CreateNew,
            stage: PendingOpenUiStage::Choosing,
            operation_active: false,
            about_passphrases_open: false,
            error: None,
        }
    }

    pub(in crate::root) fn focus_passphrase(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.passphrase_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn focus_confirmation(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.confirmation_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn clear_inputs(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        for input in [
            &self.passphrase_input,
            &self.confirmation_input,
            &self.label_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    fn sync_root_state(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (is_pending, root_stage, has_root_error) = {
            let root = self.root.read(cx);
            (
                matches!(root.vault_state, VaultState::PendingSoftwareProfileOpen),
                root.pending_software_profile_open_stage(),
                root.vault_error.is_some(),
            )
        };

        if !is_pending {
            self.clear_inputs_if_needed(window, cx);
            self.stage = PendingOpenUiStage::Choosing;
            self.operation_active = false;
            self.about_passphrases_open = false;
            self.error = None;
            self.remember_standard_context = false;
            self.intent = SoftwareContextSyncIntent::CreateNew;
            return;
        }

        if is_pending && root_stage.is_none() {
            self.operation_active = true;
        }
        if let Some(root_stage) = root_stage {
            let stage = pending_ui_stage(root_stage);
            let stage_changed = self.stage != stage;
            if stage_changed {
                self.error = None;
            }
            if stage != PendingOpenUiStage::Choosing {
                self.about_passphrases_open = false;
            }
            self.stage = stage;
            if stage_changed || stage != PendingOpenUiStage::Creation || has_root_error {
                self.operation_active = false;
            }
        }
    }

    fn clear_inputs_if_needed(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let has_input = [
            &self.passphrase_input,
            &self.confirmation_input,
            &self.label_input,
        ]
        .into_iter()
        .any(|input| !input.read(cx).value().is_empty());
        if has_input {
            self.clear_inputs(window, cx);
        }
    }

    fn submit_passphrase(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.operation_active || self.stage != PendingOpenUiStage::Choosing {
            return;
        }
        let passphrase = read_and_clear_input(&self.passphrase_input, window, cx);
        let passphrase = match exact_passphrase_for_handoff(passphrase.as_str()) {
            Ok(passphrase) => passphrase,
            Err(error) => {
                self.error = Some(Arc::from(error));
                cx.notify();
                return;
            }
        };
        self.operation_active = true;
        self.error = None;
        self.root.update(cx, |root, cx| {
            root.submit_pending_software_passphrase(passphrase, window, cx);
        });
        cx.notify();
    }

    fn submit_primary(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let passphrase = self.passphrase_input.read(cx).value();
        match pending_open_primary_route(passphrase.as_ref()) {
            PendingOpenPrimaryRoute::Standard => self.continue_without_passphrase(window, cx),
            PendingOpenPrimaryRoute::Passphrase => self.submit_passphrase(window, cx),
        }
    }

    fn continue_without_passphrase(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.operation_active || self.stage != PendingOpenUiStage::Choosing {
            return;
        }
        self.clear_inputs(window, cx);
        self.operation_active = true;
        self.error = None;
        let remember = self.remember_standard_context;
        self.root.update(cx, |root, cx| {
            root.continue_pending_without_passphrase(remember, window, cx);
        });
        cx.notify();
    }

    fn retry_passphrase(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.operation_active || self.stage != PendingOpenUiStage::UnknownDecision {
            return;
        }
        self.clear_inputs(window, cx);
        self.stage = PendingOpenUiStage::Choosing;
        self.error = None;
        self.intent = SoftwareContextSyncIntent::CreateNew;
        self.root
            .update(cx, WalletRoot::retry_pending_software_passphrase);
        self.focus_passphrase(window, cx);
        cx.notify();
    }

    fn add_passphrase_wallet(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.operation_active || self.stage != PendingOpenUiStage::UnknownDecision {
            return;
        }
        self.clear_inputs(window, cx);
        self.stage = PendingOpenUiStage::Creation;
        self.error = None;
        self.root.update(cx, |root, cx| {
            root.begin_pending_software_context_creation(cx);
        });
        self.focus_confirmation(window, cx);
        cx.notify();
    }

    fn submit_creation(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.operation_active || self.stage != PendingOpenUiStage::Creation {
            return;
        }
        let confirmation = read_and_clear_input(&self.confirmation_input, window, cx);
        let label = read_plain_input(&self.label_input, window, cx);
        if let Err(error) = validate_creation_confirmation_input(confirmation.as_str()) {
            self.error = Some(Arc::from(error));
            cx.notify();
            return;
        }

        let validation = self
            .root
            .read(cx)
            .validate_pending_software_context_creation(confirmation.as_str(), &label);
        let label = match validation {
            Ok(label) => label,
            Err(error) => {
                self.error = Some(Arc::from(error));
                cx.notify();
                return;
            }
        };

        self.operation_active = true;
        self.error = None;
        let intent = self.intent;
        self.root.update(cx, |root, cx| {
            root.prepare_pending_software_context_creation(
                confirmation,
                &label,
                intent,
                window,
                cx,
            );
        });
        cx.notify();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.clear_inputs(window, cx);
        self.operation_active = false;
        self.error = None;
        self.root.update(cx, |root, cx| {
            root.cancel_pending_software_profile_open(window, cx);
        });
    }

    fn render_choosing(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let dialog = cx.entity();
        let remember = self.remember_standard_context;
        let active = self.operation_active;
        let passphrase = self.passphrase_input.read(cx).value();
        let route = pending_open_primary_route(passphrase.as_ref());
        let has_passphrase = route == PendingOpenPrimaryRoute::Passphrase;
        let availability = pending_open_action_availability(self.stage, active);
        let remember_available =
            pending_open_remember_available(self.stage, active, has_passphrase);
        let primary_enabled = match route {
            PendingOpenPrimaryRoute::Standard => availability.continue_without_passphrase,
            PendingOpenPrimaryRoute::Passphrase => availability.submit_passphrase,
        };
        let primary_label = match route {
            PendingOpenPrimaryRoute::Standard => "Continue without passphrase",
            PendingOpenPrimaryRoute::Passphrase => "Open with passphrase",
        };
        let instruction = self
            .root
            .read(cx)
            .pending_software_profile_open_base_label()
            .map_or_else(
                || {
                    SharedString::from(
                        "Enter a mnemonic passphrase, or leave this empty to open it without a passphrase.",
                    )
                },
                |label| {
                    SharedString::from(format!(
                        "Enter a mnemonic passphrase for \"{label}\", or leave this empty to open it without a passphrase."
                    ))
                },
            );
        let primary_dialog = dialog.clone();
        let remember_dialog = dialog.clone();
        let about_dialog = dialog.clone();
        let cancel = app_button("pending-software-profile-cancel", "Cancel")
            .disabled(!availability.cancel)
            .on_click(move |_event, window, cx| {
                dialog.update(cx, |this, cx| this.cancel(window, cx));
            });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(vault_dialog_body(instruction))
            .child(app_masked_input(&self.passphrase_input, active))
            .child(
                Checkbox::new("pending-software-profile-remember-standard")
                    .label("Always open the standard wallet")
                    .checked(remember)
                    .disabled(!remember_available)
                    .on_click(move |checked, _window, cx| {
                        let checked = *checked;
                        remember_dialog.update(cx, |this, cx| {
                            this.remember_standard_context = checked;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Collapsible::new()
                    .open(self.about_passphrases_open)
                    .w_full()
                    .child(
                        div()
                            .id("pending-software-profile-about-passphrases")
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .on_click(move |_event, _window, cx| {
                                about_dialog.update(cx, |this, cx| {
                                    this.about_passphrases_open = !this.about_passphrases_open;
                                    cx.notify();
                                });
                            })
                            .child(app_muted_text("About mnemonic passphrases"))
                            .child(
                                Icon::new(if self.about_passphrases_open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                            ),
                    )
                    .content(
                        app_muted_text(
                            "A mnemonic passphrase is the BIP39 salt. Case and spacing are exact; any difference opens a different wallet. Use a strong, unique value.",
                        )
                        .whitespace_normal()
                        .line_height(px(18.0))
                        .pt(px(6.0))
                        .pl(px(12.0)),
                    ),
            )
            .children(self.render_error(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(cancel)
                    .child(
                        app_button("pending-software-profile-primary", primary_label)
                        .primary()
                        .loading(active)
                        .disabled(!primary_enabled)
                        .on_click(move |_event, window, cx| {
                            primary_dialog.update(cx, |this, cx| {
                                this.submit_primary(window, cx);
                            });
                        }),
                    ),
            )
    }

    fn render_unknown_decision(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let dialog = cx.entity();
        let active = self.operation_active;
        let retry_dialog = dialog.clone();
        let add_dialog = dialog.clone();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(vault_dialog_body(
                "This passphrase may be mistyped, or it may represent a wallet not yet added on this device.",
            ))
            .children(self.render_error(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("pending-software-profile-cancel-unknown", "Cancel")
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |this, cx| this.cancel(window, cx));
                            }),
                    )
                    .child(
                        app_button(
                            "pending-software-profile-add",
                            "Add wallet",
                        )
                        .outline()
                        .disabled(!pending_open_action_availability(self.stage, active)
                            .add_passphrase_wallet)
                        .on_click(move |_event, window, cx| {
                            add_dialog.update(cx, |this, cx| {
                                this.add_passphrase_wallet(window, cx);
                            });
                        }),
                    )
                    .child(
                        app_button("pending-software-profile-retry", "Try again")
                            .primary()
                            .disabled(!pending_open_action_availability(self.stage, active).retry)
                            .on_click(move |_event, window, cx| {
                                retry_dialog.update(cx, |this, cx| this.retry_passphrase(window, cx));
                            }),
                    ),
            )
    }

    fn render_creation(&self, cx: &Context<'_, Self>) -> gpui::Div {
        let dialog = cx.entity();
        let active = self.operation_active;
        let create_dialog = dialog.clone();
        let intent_dialog = dialog.clone();
        let create_selected = self.intent == SoftwareContextSyncIntent::CreateNew;
        let recover_selected = self.intent == SoftwareContextSyncIntent::RecoverExisting;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(vault_dialog_body(
                "The passphrase field was cleared - enter it again to confirm, then choose whether this wallet starts fresh or recovers existing funds.",
            ))
            .child(labeled_field(
                "Confirm mnemonic passphrase",
                app_masked_input(&self.confirmation_input, active),
            ))
            .child(labeled_field(
                "Wallet label",
                app_input(&self.label_input).disabled(active),
            ))
            .child(
                ButtonGroup::new("pending-software-profile-sync-intent")
                    .w_full()
                    .outline()
                    .compact()
                    .disabled(active)
                    .child(
                        app_segment_button(
                            "pending-software-profile-create-new",
                            "Create new",
                            create_selected,
                            active,
                            None,
                        )
                        .flex_1(),
                    )
                    .child(
                        app_segment_button(
                            "pending-software-profile-recover-existing",
                            "Recover existing",
                            recover_selected,
                            active,
                            None,
                        )
                        .flex_1(),
                    )
                    .on_click(move |selected, _window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let intent = if *index == 0 {
                            SoftwareContextSyncIntent::CreateNew
                        } else {
                            SoftwareContextSyncIntent::RecoverExisting
                        };
                        intent_dialog.update(cx, |this, cx| {
                            this.intent = intent;
                            this.error = None;
                            cx.notify();
                        });
                    }),
            )
            .children(self.render_error(cx))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("pending-software-profile-cancel-creation", "Cancel")
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |this, cx| this.cancel(window, cx));
                            }),
                    )
                    .child(
                        app_button("pending-software-profile-create", "Create wallet")
                            .primary()
                            .loading(active)
                            .disabled(!pending_open_action_availability(self.stage, active)
                                .create_context)
                            .on_click(move |_event, window, cx| {
                                create_dialog.update(cx, |this, cx| {
                                    this.submit_creation(window, cx);
                                });
                            }),
                    ),
            )
    }

    fn render_error(&self, cx: &Context<'_, Self>) -> Option<gpui::AnyElement> {
        self.error
            .clone()
            .or_else(|| self.root.read(cx).vault_error.clone())
            .map(|message| passphrase_error_alert("pending-software-profile-error", &message))
    }
}

impl Render for PassphraseOpenUi {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.sync_root_state(window, cx);
        match self.stage {
            PendingOpenUiStage::Choosing => self.render_choosing(cx),
            PendingOpenUiStage::UnknownDecision => self.render_unknown_decision(cx),
            PendingOpenUiStage::Creation => self.render_creation(cx),
        }
    }
}

fn passphrase_error_alert(id: &'static str, message: &str) -> gpui::AnyElement {
    Alert::error(id, message.to_string())
        .small()
        .into_any_element()
}

fn read_and_clear_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<'_, PassphraseOpenUi>,
) -> Zeroizing<String> {
    let value = Zeroizing::new(input.read(cx).value().to_string());
    input.update(cx, |input, cx| input.set_value("", window, cx));
    value
}

fn read_plain_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<'_, PassphraseOpenUi>,
) -> String {
    let value = input.read(cx).value().to_string();
    input.update(cx, |input, cx| input.set_value("", window, cx));
    value
}

const fn pending_ui_stage(stage: PendingSoftwareProfileOpenStage) -> PendingOpenUiStage {
    match stage {
        PendingSoftwareProfileOpenStage::Choosing => PendingOpenUiStage::Choosing,
        PendingSoftwareProfileOpenStage::UnknownDecision => PendingOpenUiStage::UnknownDecision,
        PendingSoftwareProfileOpenStage::CreationHandoff => PendingOpenUiStage::Creation,
    }
}

fn pending_open_action_availability(
    stage: PendingOpenUiStage,
    operation_active: bool,
) -> PendingOpenActionAvailability {
    let available = !operation_active;
    PendingOpenActionAvailability {
        continue_without_passphrase: available && stage == PendingOpenUiStage::Choosing,
        submit_passphrase: available && stage == PendingOpenUiStage::Choosing,
        retry: available && stage == PendingOpenUiStage::UnknownDecision,
        add_passphrase_wallet: available && stage == PendingOpenUiStage::UnknownDecision,
        create_context: available && stage == PendingOpenUiStage::Creation,
        cancel: true,
    }
}

const fn pending_open_primary_route(value: &str) -> PendingOpenPrimaryRoute {
    if value.is_empty() {
        PendingOpenPrimaryRoute::Standard
    } else {
        PendingOpenPrimaryRoute::Passphrase
    }
}

const fn pending_open_remember_available(
    stage: PendingOpenUiStage,
    operation_active: bool,
    has_passphrase: bool,
) -> bool {
    matches!(stage, PendingOpenUiStage::Choosing) && !operation_active && !has_passphrase
}

const fn validate_exact_passphrase_input(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        Err("Enter a mnemonic passphrase, or continue without one")
    } else {
        Ok(())
    }
}

fn exact_passphrase_for_handoff(value: &str) -> Result<Zeroizing<String>, &'static str> {
    validate_exact_passphrase_input(value)?;
    Ok(Zeroizing::new(value.to_owned()))
}

const fn validate_creation_confirmation_input(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        Err("Enter the mnemonic passphrase again")
    } else {
        Ok(())
    }
}

pub(in crate::root) fn validate_pending_context_label(
    value: &str,
    existing: &[WalletMetadataBundle],
) -> Result<String, &'static str> {
    let label = value.trim().to_owned();
    if label.is_empty() {
        return Err("Enter a wallet label");
    }
    let duplicate_key = label.to_lowercase();
    if existing
        .iter()
        .any(|metadata| metadata.label.trim().to_lowercase() == duplicate_key)
    {
        return Err("That label is already in use");
    }
    Ok(label)
}

pub(in crate::root) const fn creation_chain_baseline(
    intent: SoftwareContextSyncIntent,
    deployment_block: u64,
    current_safe_head: Option<u64>,
) -> Option<(u64, u64)> {
    match intent {
        SoftwareContextSyncIntent::CreateNew => match current_safe_head {
            Some(safe_head) => match safe_head.checked_add(1) {
                Some(start) => Some((start, safe_head)),
                None => None,
            },
            None => None,
        },
        SoftwareContextSyncIntent::RecoverExisting => {
            Some((deployment_block, deployment_block.saturating_sub(1)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_input_preserves_case_and_whitespace() {
        let input = "  Mixed Case  ";
        let handoff = exact_passphrase_for_handoff(input).expect("non-empty passphrase");
        assert_eq!(handoff.as_str(), input);
        let whitespace = exact_passphrase_for_handoff(" ").expect("whitespace passphrase");
        assert_eq!(whitespace.as_str(), " ");
    }

    #[test]
    fn labels_are_trimmed_and_duplicate_errors_are_generic() {
        let metadata = WalletMetadataBundle {
            wallet_uuid: "concealed".to_owned(),
            label: "Hidden child".to_owned(),
            derivation_index: 0,
            source: wallet_ops::vault::WalletSource::Imported,
            status: wallet_ops::vault::WalletStatus::Active,
            display_order: 0,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids: std::collections::BTreeSet::default(),
            software_context: None,
        };
        assert_eq!(
            validate_pending_context_label("  New wallet  ", std::slice::from_ref(&metadata)),
            Ok("New wallet".to_owned())
        );
        assert_eq!(
            validate_pending_context_label(" hidden CHILD ", std::slice::from_ref(&metadata)),
            Err("That label is already in use")
        );
    }

    #[test]
    fn primary_route_uses_exact_empty_semantics() {
        assert_eq!(
            pending_open_primary_route(""),
            PendingOpenPrimaryRoute::Standard
        );
        assert_eq!(
            pending_open_primary_route(" "),
            PendingOpenPrimaryRoute::Passphrase
        );
    }

    #[test]
    fn remember_standard_context_is_available_only_for_empty_choosing_input() {
        assert!(pending_open_remember_available(
            PendingOpenUiStage::Choosing,
            false,
            false,
        ));
        assert!(!pending_open_remember_available(
            PendingOpenUiStage::Choosing,
            false,
            true,
        ));
        assert!(!pending_open_remember_available(
            PendingOpenUiStage::Choosing,
            true,
            false,
        ));
        assert!(!pending_open_remember_available(
            PendingOpenUiStage::UnknownDecision,
            false,
            false,
        ));
    }
}
