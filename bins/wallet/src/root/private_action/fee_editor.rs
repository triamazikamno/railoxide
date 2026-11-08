use super::*;
use gpui::{Render, Subscription};
use gpui_component::ActiveTheme;
use ui::dialog::WalletDialogExt as _;

#[derive(Clone, PartialEq, Eq)]
struct FeeEditorContext {
    chain_id: u64,
    action_token: Address,
    amount_input: Entity<InputState>,
    fee_token: Address,
    broadcaster: BroadcasterChoice,
    custom_fee_amount: Option<U256>,
}

type ApplyFee = dyn Fn(Option<U256>, &mut Window, &mut App) -> bool;

/// The refused broadcaster attempt a retry editor starts from.
pub(in crate::root) struct BroadcasterFeeRetry {
    pub(in crate::root) generation_id: u64,
    pub(in crate::root) fee_amount: U256,
    pub(in crate::root) broadcaster: PublicBroadcasterCandidate,
}

struct BroadcasterFeeEditor {
    on_apply: Box<ApplyFee>,
    input: Entity<InputState>,
    decimals: Option<u8>,
    token_label: String,
    automatic_estimate: String,
    /// The refused attempt's fee and broadcaster label, shown when retrying.
    last_attempt: Option<(String, String)>,
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
                root.open_broadcaster_fee_editor(kind, key, None, window, cx);
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
                        shown_broadcaster: form.shown_broadcaster.clone(),
                        refused_broadcasters: form.refused_broadcasters.clone(),
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
                        shown_broadcaster: form.shown_broadcaster.clone(),
                        refused_broadcasters: form.refused_broadcasters.clone(),
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

    /// The form's estimate input, custom fee included, for a refused submission's retry.
    pub(in crate::root) fn broadcaster_retry_estimate_input(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &App,
    ) -> Option<PrivateEstimateInput> {
        let (context, mut input) = self.broadcaster_fee_editor_input(kind, key, cx)?;
        input.custom_fee_amount = context.custom_fee_amount;
        Some(input)
    }

    pub(in crate::root) fn open_broadcaster_fee_editor(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        retry: Option<BroadcasterFeeRetry>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((context, mut estimate_input)) = self.broadcaster_fee_editor_input(kind, key, cx)
        else {
            return;
        };
        // A Random retry stays with the refused broadcaster, so it is pinned rather than skipped.
        let pin = retry
            .as_ref()
            .filter(|_| context.broadcaster == BroadcasterChoice::Random)
            .map(|retry| retry.broadcaster.railgun_address.clone());
        if let Some(pin) = &pin {
            estimate_input.shown_broadcaster = Some(pin.clone());
            estimate_input
                .refused_broadcasters
                .retain(|address| address != pin);
        }
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
        let initial = retry
            .as_ref()
            .map(|retry| retry.fee_amount)
            .or(context.custom_fee_amount)
            .or(current_fee)
            .map(|amount| format_send_amount_input(amount, decimals))
            .unwrap_or_default();
        let retry_token = retry.as_ref().map(|retry| (retry.generation_id, pin));
        let last_attempt = retry.map(|retry| {
            (
                format!(
                    "{} {token_label}",
                    format_send_amount_input(retry.fee_amount, decimals)
                ),
                crate::root::broadcaster_picker::broadcaster_candidate_label(&retry.broadcaster),
            )
        });
        let request = self.prepare_private_broadcaster_estimate(&estimate_input);
        let root = cx.entity();
        let editor = cx.new(|cx| {
            BroadcasterFeeEditor::new(
                initial,
                decimals,
                token_label,
                move |amount, window, cx| {
                    root.update(cx, |root, cx| {
                        root.apply_broadcaster_fee(
                            kind,
                            key,
                            &context,
                            amount,
                            retry_token.clone(),
                            window,
                            cx,
                        )
                    })
                },
                window,
                cx,
            )
        });
        editor.update(cx, |editor, _| editor.last_attempt = last_attempt);
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

    /// `retry` is a refused submission's generation id and its pinned broadcaster, if any.
    #[allow(clippy::too_many_arguments)]
    fn apply_broadcaster_fee(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        expected: &FeeEditorContext,
        amount: Option<U256>,
        retry: Option<(u64, Option<String>)>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self
            .broadcaster_fee_editor_input(kind, key, cx)
            .is_some_and(|(current, _)| current == *expected)
        {
            return false;
        }
        let (retry_generation_id, pin) = retry.map_or((None, None), |(id, pin)| (Some(id), pin));
        match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get_mut(&key).expect("validated form");
                form.custom_fee_amount = amount;
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.estimating_cost = false;
                form.error = None;
                form.result = None;
                if let Some(pin) = &pin {
                    form.refused_broadcasters.retain(|address| address != pin);
                    form.shown_broadcaster = Some(pin.clone());
                }
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
                if let Some(pin) = &pin {
                    form.refused_broadcasters.retain(|address| address != pin);
                    form.shown_broadcaster = Some(pin.clone());
                }
            }
        }
        match retry_generation_id {
            Some(generation_id) => {
                self.start_refused_broadcaster_retry(kind, key, generation_id, pin, window, cx);
            }
            None => self.schedule_public_broadcaster_cost_estimate(kind, key, cx),
        }
        true
    }
}

fn open_fee_editor_dialog(editor: Entity<BroadcasterFeeEditor>, window: &mut Window, cx: &mut App) {
    let focus = editor.read(cx).input.clone();
    window.open_wallet_dialog(cx, move |dialog, window, _| {
        let apply = editor.clone();
        let automatic = editor.clone();
        let confirm = editor.clone();
        dialog
            .title("Transaction fee")
            .max_h(window.viewport_size().height * 0.88)
            .w(gpui::rems(30.0)
                .to_pixels(window.rem_size())
                .min(window.viewport_size().width * 0.92))
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |editor, cx| editor.apply(false, window, cx))
            })
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
                                if automatic.update(cx, |editor, cx| editor.apply(true, window, cx))
                                {
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
                                if apply.update(cx, |editor, cx| editor.apply(false, window, cx)) {
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
        on_apply: impl Fn(Option<U256>, &mut Window, &mut App) -> bool + 'static,
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
            last_attempt: None,
            error: None,
            _subscription: subscription,
        }
    }

    fn apply(&mut self, automatic: bool, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
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
        let applied = (self.on_apply)(amount, window, cx);
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
            .when_some(self.last_attempt.as_ref(), |this, (fee, broadcaster)| this
                .child(app_muted_text(format!("Last attempt: {fee} (refused)")).whitespace_normal())
                .child(app_muted_text(format!("Broadcaster: {broadcaster}")).whitespace_normal()))
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
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
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
                    move |fee, _, _| {
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
