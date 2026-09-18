use super::*;
use gpui::{Render, Subscription};
use gpui_component::ActiveTheme;

#[derive(Clone, PartialEq, Eq)]
struct FeeEditorContext {
    chain_id: u64,
    action_token: Address,
    amount_input: Entity<InputState>,
    fee_token: Address,
    broadcaster: BroadcasterChoice,
    custom_fee_amount: Option<U256>,
}

type ApplyFee = dyn Fn(Option<U256>, &mut App) -> bool;

struct BroadcasterFeeEditor {
    on_apply: Box<ApplyFee>,
    input: Entity<InputState>,
    decimals: Option<u8>,
    token_label: String,
    automatic_estimate: String,
    error: Option<String>,
    _subscription: Subscription,
}

pub(in crate::root) fn custom_fee_label(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: &EffectiveTokenRegistry,
) -> String {
    let metadata = token_display_metadata(Some(registry), chain_id, &token);
    let value = format_send_amount_input(amount, metadata.as_ref().map(|token| token.decimals));
    let symbol = metadata.map_or_else(
        || format!("raw units of {}", token.to_checksum(None)),
        |token| token.symbol,
    );
    format!("{value} {symbol}")
}

pub(in crate::root) fn fee_edit_button(
    root: Entity<WalletRoot>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
) -> Button {
    app_button_base(delivery_element_id(key, kind, "edit-transaction-fee"))
        .ghost()
        .small()
        .icon(Icon::empty().path("ui/icons/pencil.svg"))
        .accessibility_label("Edit transaction fee")
        .tooltip("Edit transaction fee…")
        .on_click(move |_, window, cx| {
            root.update(cx, |root, cx| {
                root.open_broadcaster_fee_editor(kind, key, window, cx);
            });
        })
}

impl WalletRoot {
    fn broadcaster_fee_editor_input(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &App,
    ) -> Option<(FeeEditorContext, PrivateEstimateInput)> {
        let (amount_input, custom_fee_amount, input) = match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get(&key)?;
                if form.generating
                    || form.gateway_execution.is_some()
                    || form.delivery_mode != DeliveryMode::PublicBroadcaster
                {
                    return None;
                }
                (
                    form.amount_input.clone(),
                    form.custom_fee_amount,
                    PrivateEstimateInput {
                        custom_fee_amount: None,
                        asset: form.asset.clone(),
                        recipient: form.recipient_input.read(cx).value().to_string(),
                        amount: form.amount_input.read(cx).value().to_string(),
                        broadcaster: form.broadcaster_choice.clone(),
                        fee_token: form.selected_fee_token,
                        fee_mode: form.fee_mode,
                        allow_out_of_range: form.allow_suspicious_broadcasters,
                        favorites_only: form.favorites_only_broadcasters,
                        output: PrivateEstimateOutput::Send,
                    },
                )
            }
            DeliveryFormKind::Unshield => {
                let form = self.unshield_forms.get(&key)?;
                if form.generating
                    || form.gateway_execution.is_some()
                    || form.delivery_mode != DeliveryMode::PublicBroadcaster
                {
                    return None;
                }
                (
                    form.amount_input.clone(),
                    form.custom_fee_amount,
                    PrivateEstimateInput {
                        custom_fee_amount: None,
                        asset: form.asset.clone(),
                        recipient: form.recipient_input.read(cx).value().to_string(),
                        amount: form.amount_input.read(cx).value().to_string(),
                        broadcaster: form.broadcaster_choice.clone(),
                        fee_token: form.selected_fee_token,
                        fee_mode: form.fee_mode,
                        allow_out_of_range: form.allow_suspicious_broadcasters,
                        favorites_only: form.favorites_only_broadcasters,
                        output: PrivateEstimateOutput::Unshield {
                            unwrap: form.unwrap,
                            native_top_up: form
                                .native_top_up_enabled
                                .then(|| form.native_top_up.clone())
                                .flatten(),
                        },
                    },
                )
            }
        };
        let context = FeeEditorContext {
            chain_id: input.asset.chain_id,
            action_token: input.asset.token,
            amount_input,
            fee_token: input.fee_token,
            broadcaster: input.broadcaster.clone(),
            custom_fee_amount,
        };
        Some((context, input))
    }

    fn open_broadcaster_fee_editor(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((context, estimate_input)) = self.broadcaster_fee_editor_input(kind, key, cx)
        else {
            return;
        };
        let metadata = token_display_metadata(
            Some(&self.effective_token_registry),
            key.chain_id,
            &context.fee_token,
        );
        let decimals = metadata.as_ref().map(|token| token.decimals);
        let token_label = metadata.map_or_else(
            || format!("Raw units · {}", context.fee_token.to_checksum(None)),
            |token| token.symbol,
        );
        let current_fee = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .and_then(|form| form.cost_estimate.as_ref()),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.cost_estimate.as_ref()),
        }
        .map(|estimate| estimate.fee_amount);
        let initial = context
            .custom_fee_amount
            .or(current_fee)
            .map(|amount| format_send_amount_input(amount, decimals))
            .unwrap_or_default();
        let request = self.prepare_private_broadcaster_estimate(&estimate_input);
        let root = cx.entity();
        let editor = cx.new(|cx| {
            BroadcasterFeeEditor::new(
                initial,
                decimals,
                token_label,
                move |amount, cx| {
                    root.update(cx, |root, cx| {
                        root.apply_broadcaster_fee(kind, key, &context, amount, cx)
                    })
                },
                window,
                cx,
            )
        });
        if let Ok(Some(request)) = request {
            let http = self.http.clone();
            let join = self
                .runtime
                .spawn(async move { request.estimate(&http).await });
            editor.update(cx, |_, cx| {
                cx.spawn(async move |this, cx| {
                    let result = join.await;
                    let _ = this.update(cx, |this, cx| {
                        this.automatic_estimate = match result {
                            Ok(Ok(estimate)) => {
                                format_send_amount_input(estimate.fee_amount, this.decimals)
                            }
                            _ => "Unavailable. Use automatic estimation to retry.".into(),
                        };
                        cx.notify();
                    });
                })
                .detach();
            });
        } else {
            editor.update(cx, |editor, cx| {
                editor.automatic_estimate =
                    "Unavailable until the transaction details are complete.".into();
                cx.notify();
            });
        }
        open_fee_editor_dialog(editor, window, cx);
    }

    fn apply_broadcaster_fee(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        expected: &FeeEditorContext,
        amount: Option<U256>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self
            .broadcaster_fee_editor_input(kind, key, cx)
            .is_some_and(|(current, _)| current == *expected)
        {
            return false;
        }
        match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get_mut(&key).expect("validated form");
                form.custom_fee_amount = amount;
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.estimating_cost = false;
                form.error = None;
                form.result = None;
            }
            DeliveryFormKind::Unshield => {
                let form = self.unshield_forms.get_mut(&key).expect("validated form");
                form.custom_fee_amount = amount;
                form.executor_review = None;
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.estimating_cost = false;
                form.error = None;
                form.result = None;
            }
        }
        self.schedule_public_broadcaster_cost_estimate(kind, key, cx);
        true
    }
}

fn open_fee_editor_dialog(editor: Entity<BroadcasterFeeEditor>, window: &mut Window, cx: &mut App) {
    let focus = editor.read(cx).input.clone();
    window.open_dialog(cx, move |dialog, window, _| {
        let apply = editor.clone();
        let automatic = editor.clone();
        let confirm = editor.clone();
        dialog
            .title("Transaction fee")
            .max_h(window.viewport_size().height * 0.88)
            .w(gpui::rems(30.0)
                .to_pixels(window.rem_size())
                .min(window.viewport_size().width * 0.92))
            .on_ok(move |_, _, cx| confirm.update(cx, |editor, cx| editor.apply(false, cx)))
            .child(editor.clone())
            .footer(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("automatic-broadcaster-fee", "Use automatic")
                            .debug_selector(|| "automatic-broadcaster-fee".into())
                            .on_click(move |_, window, cx| {
                                if automatic.update(cx, |editor, cx| editor.apply(true, cx)) {
                                    window.close_dialog(cx);
                                }
                            }),
                    )
                    .child(
                        app_button("cancel-broadcaster-fee", "Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        app_button("apply-broadcaster-fee", "Apply")
                            .debug_selector(|| "apply-broadcaster-fee".into())
                            .primary()
                            .on_click(move |_, window, cx| {
                                if apply.update(cx, |editor, cx| editor.apply(false, cx)) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
    focus.read(cx).focus_handle(cx).focus(window, cx);
}

impl BroadcasterFeeEditor {
    fn new(
        initial: String,
        decimals: Option<u8>,
        token_label: String,
        on_apply: impl Fn(Option<U256>, &mut App) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                cx.notify();
            }
        });
        Self {
            on_apply: Box::new(on_apply),
            input,
            decimals,
            token_label,
            automatic_estimate: "Calculating…".into(),
            error: None,
            _subscription: subscription,
        }
    }

    fn apply(&mut self, automatic: bool, cx: &mut Context<'_, Self>) -> bool {
        let amount = if automatic {
            None
        } else {
            match parse_send_amount(self.input.read(cx).value().as_ref(), self.decimals) {
                Ok(amount) if !amount.is_zero() => Some(amount),
                Ok(_) => {
                    self.error = Some("Enter a fee greater than zero.".into());
                    cx.notify();
                    return false;
                }
                Err(error) => {
                    self.error = Some(error.to_string());
                    cx.notify();
                    return false;
                }
            }
        };
        let applied = (self.on_apply)(amount, cx);
        if !applied {
            self.error = Some("The transaction changed. Close this editor and reopen it.".into());
            cx.notify();
        }
        applied
    }
}

impl Render for BroadcasterFeeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().flex().flex_col().gap_3().min_w_0()
            .child(app_muted_text("Set the total payment for this transaction, including gas and the broadcaster's fee. The RAILGUN protocol fee is separate.").whitespace_normal())
            .child(app_muted_text(format!("Automatic estimate: {}", self.automatic_estimate)).whitespace_normal())
            .child(div().flex().flex_col().gap_1()
                .child(app_strong_text(format!("Total transaction fee · {}", self.token_label)))
                .child(app_input(&self.input).aria_label("Total transaction fee"))
                .when_some(self.error.as_ref(), |this, error| this.child(app_muted_text(error.clone()).text_color(cx.theme().danger).whitespace_normal())))
            .child(app_muted_text("The full custom amount will be paid. If it is insufficient, submission stops for another review.").whitespace_normal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::Root;
    use std::{cell::RefCell, rc::Rc};

    struct DialogHost;

    impl Render for DialogHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .size_full()
                .children(Root::render_dialog_layer(window, cx))
        }
    }

    #[gpui::test]
    fn custom_fee_dialog_validates_applies_and_returns_to_automatic(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(ui::theme::apply_zenburn_component_theme);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| DialogHost);
            Root::new(view, window, cx)
        });
        let applied = Rc::new(RefCell::new(Vec::new()));
        let changes = applied.clone();
        let editor = cx.update(|window, cx| {
            let editor = cx.new(|cx| {
                BroadcasterFeeEditor::new(
                    String::new(),
                    Some(6),
                    "USDC".into(),
                    move |fee, _| {
                        changes.borrow_mut().push(fee);
                        true
                    },
                    window,
                    cx,
                )
            });
            open_fee_editor_dialog(editor.clone(), window, cx);
            window.draw(cx).clear(cx);
            editor
        });
        cx.simulate_input("0");
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| {
            assert!(editor.read(cx).error.is_some());
            assert!(applied.borrow().is_empty());
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("0.0002");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let apply = cx
            .debug_bounds("apply-broadcaster-fee")
            .expect("Apply button");
        cx.simulate_click(apply.center(), gpui::Modifiers::none());
        assert_eq!(*applied.borrow(), vec![Some(U256::from(200))]);

        cx.update(|window, cx| {
            open_fee_editor_dialog(editor.clone(), window, cx);
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(
            applied.borrow().len(),
            1,
            "Cancel must leave the fee unchanged"
        );
        cx.update(|window, cx| {
            open_fee_editor_dialog(editor, window, cx);
            window.draw(cx).clear(cx);
        });
        let automatic = cx
            .debug_bounds("automatic-broadcaster-fee")
            .expect("Use automatic button");
        cx.simulate_click(automatic.center(), gpui::Modifiers::none());
        assert_eq!(*applied.borrow(), vec![Some(U256::from(200)), None]);
    }
}
