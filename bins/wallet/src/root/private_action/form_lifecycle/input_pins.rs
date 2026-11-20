#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Runs `change` once the user accepts that an earlier proof of this payment may still
    /// execute alongside the next one. Cancel leaves everything as it was.
    pub(in crate::root) fn confirm_double_payment_risk(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        change: impl Fn(&mut Self, &mut Window, &mut Context<'_, Self>) + 'static,
    ) {
        let root = cx.entity();
        let change = Rc::new(change);
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
            let root = root.clone();
            let change = Rc::clone(&change);
            confirmation_dialog(dialog, DOUBLE_PAYMENT_RISK, dialog_width, dialog_max_height).on_ok(
                move |_event, window, cx| {
                    // After this dialog closes, so a dialog `change` opens stays open.
                    let (root, change) = (root.clone(), Rc::clone(&change));
                    window.defer(cx, move |window, cx| {
                        root.update(cx, |root, cx| change(root, window, cx));
                    });
                    true
                },
            )
        });
    }

    /// A pinned form's change that takes the next attempt off its pinned notes runs only after
    /// the user accepts the risk, which also clears the pin. Returns whether it asked.
    pub(in crate::root) fn confirm_leaving_input_pin(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        change: impl Fn(&mut Self, &mut Window, &mut Context<'_, Self>) + 'static,
    ) -> bool {
        let pinned = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .is_some_and(|form| form.input_pin.is_some()),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .is_some_and(|form| form.input_pin.is_some()),
        };
        if !pinned {
            return false;
        }
        Self::confirm_double_payment_risk(window, cx, move |root, window, cx| {
            match kind {
                DeliveryFormKind::Send => {
                    if let Some(form) = root.send_forms.get_mut(&key) {
                        form.input_pin = None;
                        form.input_pin_conflict = false;
                    }
                }
                DeliveryFormKind::Unshield => {
                    if let Some(form) = root.unshield_forms.get_mut(&key) {
                        form.input_pin = None;
                        form.input_pin_conflict = false;
                    }
                }
            }
            change(root, window, cx);
        });
        true
    }

    /// Before a submission from a pinned form, asks about an attempt that can't stay on its
    /// pinned notes: one off the broadcaster path, or one the estimate found the notes can't
    /// make. Continuing the latter overrides the pin for one attempt and refreshes the
    /// estimate. Returns whether it asked.
    pub(in crate::root) fn confirm_private_input_pin_admission(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let (pin, conflict, leaves_pin) = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key).filter(|form| !form.generating) else {
                    return false;
                };
                (
                    form.input_pin.as_ref(),
                    form.input_pin_conflict,
                    form.delivery_mode != DeliveryMode::PublicBroadcaster,
                )
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self
                    .unshield_forms
                    .get(&key)
                    .filter(|form| !form.generating)
                else {
                    return false;
                };
                (
                    form.input_pin.as_ref(),
                    form.input_pin_conflict,
                    // Unwrap and gas top-up go through the executor, which a pin can't bind.
                    form.delivery_mode != DeliveryMode::PublicBroadcaster
                        || form.unwrap
                        || (form.native_top_up_enabled && form.native_top_up.is_some()),
                )
            }
        };
        let Some(pin) = pin else {
            return false;
        };
        if leaves_pin {
            return self.confirm_leaving_input_pin(
                kind,
                key,
                window,
                cx,
                move |root, window, cx| match kind {
                    DeliveryFormKind::Send => {
                        root.generate_send_calldata_from_form(key, window, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.generate_unshield_calldata_from_form(key, window, cx);
                    }
                },
            );
        }
        if !conflict || pin.overridden {
            return false;
        }
        Self::confirm_double_payment_risk(window, cx, move |root, _window, cx| {
            let pin = match kind {
                DeliveryFormKind::Send => root.send_forms.get_mut(&key).and_then(|form| {
                    form.input_pin_conflict = false;
                    form.input_pin.as_mut()
                }),
                DeliveryFormKind::Unshield => root.unshield_forms.get_mut(&key).and_then(|form| {
                    form.input_pin_conflict = false;
                    form.input_pin.as_mut()
                }),
            };
            if let Some(pin) = pin {
                pin.overridden = true;
            }
            root.schedule_public_broadcaster_cost_estimate(kind, key, cx);
        });
        true
    }
}
