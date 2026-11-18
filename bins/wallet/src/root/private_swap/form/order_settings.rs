use super::*;

impl PrivateSwapsView {
    pub(super) fn set_slippage(
        &mut self,
        bps: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.slippage_bps == bps || form.speed_up.is_some() {
            return;
        }
        form.slippage_bps = bps;
        self.schedule_quote(window, cx);
        // The popover lives in the quote details, which only a ready quote shows. Move focus
        // to the amount before they go, so keyboard input and Escape still reach the dialog.
        if self
            .form
            .as_ref()
            .is_some_and(|form| !matches!(form.quote, QuoteState::Ready(_)))
        {
            self.focus_form_amount(window, cx);
        }
        cx.notify();
    }

    /// Choose a gas share preset. It closes the bar and the Minimum field.
    pub(super) fn set_gas_preset(
        &mut self,
        preset: GasPreset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.close_gas_minimum(window, cx);
        self.set_gas_share(preset.share_bps(), false, window, cx);
    }

    /// Stop showing the bar and the Minimum field for the edit button. Focus on either moves
    /// to the amount before they go, so keyboard input and Escape still reach the dialog.
    fn close_gas_minimum(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.gas_minimum_editing = false;
        if form.gas_bar_focus.is_focused(window)
            || form
                .gas_minimum_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            self.focus_form_amount(window, cx);
        }
    }

    /// The edit button: open the bar and the Minimum field, with the field focused, or close
    /// them. A custom share keeps them open, and the button then only focuses the field. The
    /// share and its preset stay until the knob or the field moves it.
    pub(super) fn edit_gas_minimum(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.gas_minimum_editing && !form.gas_custom {
            self.close_gas_minimum(window, cx);
            cx.notify();
            return;
        }
        form.gas_minimum_editing = true;
        let input = form.gas_minimum_input.clone();
        // The field takes the shown minimum before it is focused, as a focused one keeps its text.
        self.sync_gas_controls(window, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        // Once the field is drawn, typing replaces the shown minimum instead of appending to it.
        window.on_next_frame(|window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        });
        cx.notify();
    }

    /// Choose the share of the gas estimate the minimum deducts. A same-chain quote is priced
    /// again locally, without a request. A Bridge swap's bridge leg was quoted for the old
    /// order amount, so a change of the share the strip shows quotes that leg again; until
    /// then the strip shows the local price, and the swap can't be reviewed. A change clears
    /// consent to high costs.
    pub(super) fn set_gas_share(
        &mut self,
        share_bps: u16,
        custom: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let share_bps = share_bps.min(GAS_SHARE_LOOSE_BPS);
        let shown = form.shown_gas_share();
        let changed = (form.gas_share_bps, form.gas_custom) != (share_bps, custom);
        if changed {
            form.gas_share_bps = share_bps;
            form.gas_custom = custom;
            form.high_costs_acknowledged = false;
            form.continuing_acknowledged = false;
            form.error = None;
        }
        // A swap paid from a Public account is priced by its own review.
        if form.public.is_some() {
            if changed {
                self.public_gas_share_changed(window, cx);
            }
            self.sync_gas_controls(window, cx);
            cx.notify();
            return;
        }
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.gas_share_bps = Some(share_bps);
        }
        if !form.bridged() {
            let repriced = match &form.quote {
                QuoteState::Ready(review) if review.gas_share_bps() != share_bps => {
                    review.with_gas_share(share_bps).ok()
                }
                _ => None,
            };
            if let Some(repriced) = repriced {
                form.quote = QuoteState::Ready(Arc::new(repriced));
            }
        }
        // A release at the dragged share, or a share the quote can't support, shows the same
        // share and starts no refresh.
        let refresh = form.bridged() && form.shown_gas_share() != shown;
        self.sync_gas_controls(window, cx);
        if refresh {
            self.schedule_bridge_quote(window, cx);
        }
        cx.notify();
    }

    /// The gas bar moved to `value`, its position in percent from "you pay all gas". Moving
    /// the bar selects Custom.
    pub(super) fn move_gas_bar(
        &mut self,
        value: f32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(strip) = self.form.as_ref().and_then(public_strip) {
            let share_bps = strip.bar.share_at(slider_percent(value) * 100);
            self.set_gas_share(share_bps, true, window, cx);
            return;
        }
        let Some(QuoteState::Ready(review)) = self.form.as_ref().map(|form| &form.quote) else {
            return;
        };
        let share_bps = GasBar::of(review).share_at(slider_percent(value) * 100);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// Move the gas bar's knob from its current step, in percent of the bar, by the keyboard.
    pub(super) fn step_gas_bar(
        &mut self,
        step: impl FnOnce(u16) -> u16,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if let Some(strip) = public_strip(form) {
            let current = nearest_step(strip.bar.position_bps(strip.share_bps));
            let share_bps = strip.bar.share_at(step(current).min(100) * 100);
            self.set_gas_share(share_bps, true, window, cx);
            return;
        }
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let shown = strip_review(form, review);
        let bar = GasBar::of(&shown);
        let current = nearest_step(bar.position_bps(shown.gas_share_bps()));
        let share_bps = bar.share_at(step(current).min(100) * 100);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// The Minimum field changed: the share whose minimum it is, clamped to the bar. A later
    /// quote keeps the share, not the typed amount.
    pub(super) fn gas_minimum_edited(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if let Some(strip) = public_strip(form) {
            // The field is in the destination token. The order carries no shield, so its
            // minimum is its amount before fees.
            let Ok(shown) = wallet_ops::parse_unshield_amount(
                &form.gas_minimum_input.read(cx).value(),
                self.public_strip_decimals(form, cx),
            ) else {
                return;
            };
            let share_bps = strip.bar.share_for_pre_fee(strip.scale.order(shown));
            self.set_gas_share(share_bps, true, window, cx);
            return;
        }
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let Ok(shown) = wallet_ops::parse_unshield_amount(
            &form.gas_minimum_input.read(cx).value(),
            self.strip_decimals(form, review, cx),
        ) else {
            return;
        };
        let minimum = StripScale::of(review).order(shown);
        // For Private delivery, the amount before the shield fee.
        let pre_fee = review.buy_amount_for(minimum).unwrap_or(minimum);
        let share_bps = GasBar::of(review).share_for_pre_fee(pre_fee);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// Put the knob at the shown share's nearest step, and the Minimum field at the shown
    /// minimum unless the user is typing in it.
    pub(super) fn sync_gas_controls(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // The knob's step, the shown minimum, and the decimals of its token.
        let (step, minimum, decimals) = if let Some(strip) = public_strip(form) {
            (
                nearest_step(strip.bar.position_bps(strip.share_bps)),
                strip.scale.show(strip.limit.min_received),
                self.public_strip_decimals(form, cx),
            )
        } else {
            let QuoteState::Ready(review) = &form.quote else {
                return;
            };
            let shown = strip_review(form, review);
            (
                nearest_step(GasBar::of(&shown).position_bps(shown.gas_share_bps())),
                StripScale::of(review).show(shown.suggested_private_minimum()),
                self.strip_decimals(form, review, cx),
            )
        };
        let value = f32::from(step);
        let input = form.gas_minimum_input.clone();
        // At the display precision of the card's amounts; the share, not this text, sets the order.
        let text = (!input.read(cx).focus_handle(cx).is_focused(window)).then(|| match decimals {
            Some(decimals) => railgun_ui::format_token_amount(minimum, decimals),
            None => format_unshield_amount_input(minimum, None),
        });
        form.gas_slider.update(cx, |slider, cx| {
            slider.set_value(value, window, cx);
        });
        if let Some(text) = text {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
    }

    /// Choose how long the order is valid after signing. Bridge delivery keeps the profile's
    /// window. `validTo` is set from the quote, so the swap is quoted again.
    pub(super) fn set_valid_for(
        &mut self,
        valid_for: Duration,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.valid_for == valid_for || form.bridged() || form.speed_up.is_some() {
            return;
        }
        form.valid_for = valid_for;
        self.schedule_quote(window, cx);
        // The choice lives in the quote details, which hide until a new quote is ready, as
        // Price tolerance does.
        if self
            .form
            .as_ref()
            .is_some_and(|form| !matches!(form.quote, QuoteState::Ready(_)))
        {
            self.focus_form_amount(window, cx);
        }
        cx.notify();
    }
}
