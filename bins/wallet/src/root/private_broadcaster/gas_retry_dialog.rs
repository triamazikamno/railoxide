#[allow(clippy::wildcard_imports)]
use super::*;

impl SelfBroadcastGasRetryDialogContent {
    pub(in crate::root) fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (max_fee, max_tip) = match self.gas_inputs.parse(cx) {
            Ok(values) => values,
            Err(error) => {
                self.error = Some(Arc::from(error));
                cx.notify();
                return;
            }
        };
        self.root.update(cx, |root, cx| {
            root.submit_self_broadcast_gas_retry(
                self.kind,
                self.key,
                self.generation_id,
                self.retry_kind,
                max_fee,
                max_tip,
                cx,
            );
        });
        window.close_dialog(cx);
    }

    pub(super) fn new(
        root: Entity<WalletRoot>,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        initial_max_fee_per_gas: u128,
        initial_max_priority_fee_per_gas: u128,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let gas_inputs = GasRetryInputs::new(
            initial_max_fee_per_gas,
            initial_max_priority_fee_per_gas,
            window,
            cx,
        );
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        gas_inputs.subscribe_clear_error(cx, |this, cx| {
            this.error = None;
            cx.notify();
        });
        Self {
            root,
            kind,
            key,
            generation_id,
            retry_kind,
            gas_inputs,
            error: None,
        }
    }
}

impl gpui::Render for SelfBroadcastGasRetryDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let title = match self.retry_kind {
            SelfBroadcastGasRetryKind::RetryStep => "Retry step",
            SelfBroadcastGasRetryKind::RetryEstimate => "Retry with custom gas",
            SelfBroadcastGasRetryKind::SpeedUp => "Speed up transaction",
        };
        let detail = match self.retry_kind {
            SelfBroadcastGasRetryKind::RetryStep => {
                "Retry signing and sending this self-broadcast step with the current gas fee values."
            }
            SelfBroadcastGasRetryKind::RetryEstimate => {
                "Retry gas estimation and signing using these EIP-1559 fee values."
            }
            SelfBroadcastGasRetryKind::SpeedUp => {
                "Uses the same nonce to replace the pending transaction. Values are prefilled +12.5%."
            }
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_strong_text(title))
            .child(app_muted_text(detail).whitespace_normal())
            .child(self.gas_inputs.render_fields())
            .when_some(self.error.as_ref(), |this, error| {
                this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("self-broadcast-gas-retry-cancel", "Cancel")
                            .flex_none()
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button("self-broadcast-gas-retry-confirm", "Submit")
                            .primary()
                            .flex_none()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.submit(window, cx);
                            })),
                    ),
            )
    }
}
