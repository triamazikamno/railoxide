//! Shared chain editor inside the extension's current authenticated view.
use super::*;
use railgun_ui::chain_editor::{ChainEditorSnapshot, NativeUsdProbe};
use ui::chain_editor::{ChainEditor, ChainEditorEvent};

pub(super) struct ChainManagement {
    pub(super) editor: Entity<ChainEditor>,
    pub(super) supported: bool,
    pub(super) token: Option<String>,
    pub(super) pricing_status:
        std::collections::BTreeMap<String, railgun_ui::chain_editor::NativeUsdStatus>,
    sequence: u64,
    inspect_on_open: Option<u64>,
    _subscription: Subscription,
    _callback: Closure<dyn FnMut(JsValue)>,
}

impl ChainManagement {
    pub(super) fn new(window: &Window, cx: &mut Context<'_, GatewayView>) -> Self {
        let editor = cx.new(|cx| ChainEditor::new(ChainEditorSnapshot::default(), cx));
        let subscription = cx.subscribe_in(
            &editor,
            window,
            |view, _, event: &ChainEditorEvent, _, _| {
                let Some(token) = view.chain_management.token.clone() else {
                    return;
                };
                if !view.chain_management.supported || view.status != "unlocked" {
                    return;
                }
                view.chain_management.sequence += 1;
                host_command(
                    "chain_editor",
                    &serde_json::json!({
                        "action":"run", "editor_token":token,
                        "request_id":format!("{token}:{}", view.chain_management.sequence),
                        "revision":event.revision, "command":event.command,
                    })
                    .to_string(),
                );
            },
        );
        let owner = cx.entity().downgrade();
        let handle = window.window_handle();
        let mut app = cx.to_async();
        let callback = Closure::new(move |message: JsValue| {
            let token = text_field(&message, "editor_token");
            let generation = chain_id_field(&message, "generation");
            let outcome = field(&message, "outcome");
            let status = text_field(&outcome, "status");
            let serialized = js_sys::JSON::stringify(&outcome)
                .ok()
                .and_then(|text| text.as_string());
            let _ = app.update_window(handle, |_, window, cx| {
                let _ = owner.update(cx, |view, cx| {
                    if view.chain_management.token.as_deref() != Some(token.as_str()) {
                        return;
                    }
                    if status == "retired" {
                        view.retire_chain_editor(cx);
                        view.focus.focus(window, cx);
                        return;
                    }
                    if view.status != "unlocked"
                        || view.generation != generation
                        || !view.chain_management.supported
                    {
                        return;
                    }
                    let value = serialized
                        .as_deref()
                        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok());
                    let result = match value {
                        // A Test result is unsaved feedback: it never replaces the draft.
                        Some(mut value) if status == "probed" => {
                            if let Ok(probe) =
                                serde_json::from_value::<NativeUsdProbe>(value["probe"].take())
                            {
                                view.chain_management
                                    .editor
                                    .update(cx, |editor, cx| editor.receive_probe(probe, cx));
                                cx.notify();
                            }
                            return;
                        }
                        Some(mut value) if status == "ready" => {
                            serde_json::from_value::<ChainEditorSnapshot>(value["snapshot"].take())
                                .map_err(|_| {
                                    "Chain settings could not be read. Close and reopen the editor."
                                        .to_owned()
                                })
                        }
                        Some(value) if status == "failed" => Err(value["message"]
                            .as_str()
                            .unwrap_or("Chain settings could not be saved")
                            .to_owned()),
                        _ => return,
                    };
                    let inspect = view
                        .chain_management
                        .inspect_on_open
                        .take()
                        .filter(|_| result.is_ok());
                    view.chain_management.editor.update(cx, |editor, cx| {
                        editor.receive(result, window, cx);
                        if let Some(chain_id) = inspect {
                            editor.inspect_chain(chain_id, cx);
                        }
                    });
                    cx.notify();
                });
            });
        });
        host_subscribe_chain_editor(callback.as_ref().unchecked_ref());
        Self {
            editor,
            supported: false,
            token: None,
            pricing_status: std::collections::BTreeMap::new(),
            sequence: 0,
            inspect_on_open: None,
            _subscription: subscription,
            _callback: callback,
        }
    }
}

impl GatewayView {
    pub(super) fn retire_chain_editor(&mut self, cx: &mut Context<'_, Self>) {
        self.chain_management.token = None;
        self.chain_management.inspect_on_open = None;
        self.chain_management.editor.update(cx, ChainEditor::retire);
        cx.notify();
    }

    pub(super) fn open_chain_editor(
        &mut self,
        chain_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.status != "unlocked"
            || !self.chain_management.supported
            || self.chain_management.token.is_some()
        {
            return;
        }
        self.chain_management.sequence += 1;
        let token = format!("chains:{}", self.chain_management.sequence);
        self.chain_management.token = Some(token.clone());
        self.chain_management.inspect_on_open = chain_id;
        let status = self.chain_management.pricing_status.clone();
        self.chain_management
            .editor
            .update(cx, |editor, cx| editor.set_pricing_status(status, cx));
        host_command(
            "chain_editor",
            &serde_json::json!({"action":"open", "editor_token":token}).to_string(),
        );
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn close_chain_editor(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(token) = self.chain_management.token.as_ref() {
            host_command(
                "chain_editor",
                &serde_json::json!({"action":"close", "editor_token":token}).to_string(),
            );
        }
        self.retire_chain_editor(cx);
        self.focus.focus(window, cx);
    }

    pub(super) fn render_chain_management(&self, cx: &Context<'_, Self>) -> AnyElement {
        div()
            .id("gateway-chain-management")
            .track_focus(&self.focus)
            .key_context("GatewayView")
            .on_action(cx.listener(|this, _: &public_view::Back, window, cx| {
                this.close_chain_editor(window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .font_family(SANS)
            .text_size(theme::APP_TEXT_SIZE)
            .child(div().p_3().child(
                app_button("gateway-chains-back", "Back").on_click(
                    cx.listener(|this, _, window, cx| this.close_chain_editor(window, cx)),
                ),
            ))
            .child(
                // The list renders at its natural height, so this region scrolls it.
                div()
                    .id("gateway-chain-management-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.chain_management.editor.clone()),
            )
            .into_any_element()
    }
}
