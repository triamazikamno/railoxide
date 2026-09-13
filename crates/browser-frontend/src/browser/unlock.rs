use super::*;
use gpui_component::{ActiveTheme as _, input::InputEvent, spinner::Spinner};
use ui::{controls::app_masked_input, icons};

pub(super) struct UnlockForm {
    pub(super) input: Entity<InputState>,
    allowed: bool,
    phase: String,
    generation: Option<u64>,
}

impl UnlockForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, GatewayView>) -> Self {
        // Host status can clear this input before its first render establishes a font.
        // WASM can resolve only the packaged fonts, not the native system fallback.
        let input = window.with_text_style(
            Some(gpui::TextStyleRefinement {
                font_family: Some(cx.theme().font_family.clone()),
                ..Default::default()
            }),
            |window| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(true)
                        .placeholder("vault password")
                })
            },
        );
        Self {
            input,
            allowed: false,
            phase: String::new(),
            generation: None,
        }
    }

    pub(super) fn subscriptions(
        &self,
        window: &Window,
        cx: &mut Context<'_, GatewayView>,
    ) -> Vec<Subscription> {
        vec![
            cx.observe(&self.input, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &self.input,
                window,
                |view, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.submit_unlock_input(window, cx);
                    }
                },
            ),
        ]
    }

    pub(super) fn sync(
        &mut self,
        state: &JsValue,
        window: &mut Window,
        cx: &mut Context<'_, GatewayView>,
    ) {
        let unlock = field(state, "unlock");
        let allowed = text_field(state, "status") == "locked" && flag_field(&unlock, "allowed");
        let phase = text_field(&unlock, "phase");
        let generation = chain_id_field(&unlock, "generation");
        if !allowed || self.phase != phase || self.generation != generation {
            self.input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.set_placeholder(
                    if phase == "passphrase" {
                        "mnemonic passphrase"
                    } else {
                        "vault password"
                    },
                    window,
                    cx,
                );
            });
            if allowed
                && matches!(
                    phase.as_str(),
                    "password" | "passphrase" | "failed" | "cancelled"
                )
            {
                self.input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
        self.allowed = allowed;
        self.phase = phase;
        self.generation = generation;
    }
}

impl GatewayView {
    fn send_unlock_action(
        &mut self,
        action: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let value = self.unlock.input.read(cx).value().to_string();
        self.unlock
            .input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let mut command =
            serde_json::json!({"action": action, "generation": self.unlock.generation});
        match action {
            "password" => {
                command["password"] = value.into();
            }
            "passphrase" => {
                command["passphrase"] = value.into();
            }
            _ => {}
        }
        host_command("unlock", &command.to_string());
        self.unlock.phase = "busy".into();
        cx.notify();
    }

    pub(super) fn submit_unlock_input(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.unlock.allowed {
            return;
        }
        let value = self.unlock.input.read(cx).value();
        if value.len() > 4096 {
            return;
        }
        let empty = value.is_empty();
        let action = match self.unlock.phase.as_str() {
            "password" | "failed" | "cancelled" | "rate_limited" if !empty => "password",
            "passphrase" if empty => "standard",
            "passphrase" => "passphrase",
            _ => return,
        };
        self.send_unlock_action(action, window, cx);
    }

    pub(super) fn render_unlock(&self, cx: &Context<'_, Self>) -> Option<Div> {
        if !self.unlock.allowed {
            return None;
        }
        let phase = self.unlock.phase.as_str();
        let passphrase = phase == "passphrase";
        let value = self.unlock.input.read(cx).value();
        let too_long = value.len() > 4096;
        let busy = matches!(phase, "busy" | "opening");
        let message = match phase {
            "failed" => Some("Could not unlock. Check your vault password and try again."),
            "cancelled" => Some("Unlock cancelled. Enter your vault password to try again."),
            "rate_limited" => Some("Too many attempts. Wait a minute before trying again."),
            "unknown" => Some(
                "This passphrase does not match a wallet added on this desktop. Try again, or use the desktop to add or recover a wallet.",
            ),
            "unavailable" => Some("Unlock is unavailable. Continue in the desktop app."),
            "desktop" => Some("Continue in the desktop app to finish opening your wallet."),
            "other" => {
                Some("Another wallet window is unlocking. Continue there or open the desktop app.")
            }
            _ => None,
        };
        let input = matches!(
            phase,
            "password" | "passphrase" | "failed" | "cancelled" | "rate_limited"
        );
        let label = if passphrase && self.unlock.input.read(cx).value().is_empty() {
            "Continue without passphrase"
        } else if passphrase {
            "Open with passphrase"
        } else {
            "Unlock"
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .flex_1()
                .min_w_0()
                .when(passphrase, |view| {
                    view.child(
                        app_strong_text("Open software wallet")
                            .w_full()
                            .text_center(),
                    )
                })
                .when_some(message, |view, message| view.child(note(message)))
                .when(busy, |view| {
                    view.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Spinner::new())
                            .child(note("Opening wallet…")),
                    )
                })
                .when(input, |view| {
                    view.when(!passphrase, |view| {
                        view.child(
                            div().w_full().flex().justify_center().child(
                                Icon::empty()
                                    .path(icons::lock_icon_path())
                                    .size(rems(15.0))
                                    .text_color(cx.theme().muted_foreground),
                            ),
                        )
                    })
                    .child(app_masked_input(&self.unlock.input, false))
                    .when(too_long, |view| view.child(note("This input is too long.")))
                    .when(passphrase, |view| {
                        view.child(note(
                            "Enter your mnemonic passphrase, or continue without one.",
                        ))
                    })
                    .child(
                        app_button("extension-unlock-submit", label)
                            .primary()
                            .disabled(too_long || (!passphrase && value.is_empty()))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.submit_unlock_input(window, cx);
                            })),
                    )
                })
                .when(phase == "unknown", |view| {
                    view.child(app_button("extension-unlock-retry", "Try again").on_click(
                        cx.listener(|view, _, window, cx| {
                            view.send_unlock_action("retry", window, cx);
                        }),
                    ))
                })
                .when(matches!(phase, "passphrase" | "unknown"), |view| {
                    view.child(
                        app_button("extension-unlock-desktop", "Continue in desktop app").on_click(
                            cx.listener(|view, _, window, cx| {
                                view.send_unlock_action("desktop", window, cx);
                            }),
                        ),
                    )
                })
                .when(busy || passphrase || phase == "unknown", |view| {
                    view.child(app_button("extension-unlock-cancel", "Cancel").on_click(
                        cx.listener(|view, _, window, cx| {
                            view.send_unlock_action("cancel", window, cx);
                        }),
                    ))
                }),
        )
    }
}
