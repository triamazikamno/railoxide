#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// The user's explicit refresh: Random may pick a different broadcaster than the shown one.
    pub(in crate::root) fn reroll_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get_mut(&key)
                .map(|form| &mut form.shown_broadcaster),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get_mut(&key)
                .map(|form| &mut form.shown_broadcaster),
        };
        if let Some(shown) = shown {
            *shown = None;
        }
        self.schedule_public_broadcaster_cost_estimate(kind, key, cx);
    }

    pub(in crate::root) fn schedule_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        self.start_public_broadcaster_cost_estimate(kind, key, false, cx);
    }

    /// A background refresh keeps the shown estimate, and its submission, until the new one lands.
    pub(in crate::root) fn revalidate_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        self.start_public_broadcaster_cost_estimate(kind, key, true, cx);
    }

    fn start_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        retain_shown: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.can_schedule_public_broadcaster_cost_estimate(kind, key) {
            // Debug UI fixture: in a `delivery:` mode, the refusal drops the stand-in quote.
            #[cfg(debug_assertions)]
            self.drop_ui_fixture_delivery_estimate(kind, key, retain_shown, cx);
            return;
        }

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    if !retain_shown {
                        form.cost_estimate = None;
                    }
                    form.cost_estimate_pending = false;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.replaced_estimate = if retain_shown {
                        None
                    } else {
                        form.cost_estimate.take()
                    };
                    form.cost_estimate_pending = false;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
        }
        cx.notify();

        match kind {
            DeliveryFormKind::Send => self.estimate_send_public_broadcaster_cost_from_form(key, cx),
            DeliveryFormKind::Unshield => {
                self.estimate_unshield_public_broadcaster_cost_from_form(key, cx);
            }
        }
        if !retain_shown {
            return;
        }
        // A retained estimate whose refresh could not start, e.g. mid-sync, is no longer current.
        let unrefreshed = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get_mut(&key)
                .filter(|form| !form.estimating_cost)
                .map(|form| &mut form.cost_estimate),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get_mut(&key)
                .filter(|form| !form.estimating_cost)
                .map(|form| &mut form.cost_estimate),
        };
        if let Some(estimate) = unrefreshed
            && estimate.take().is_some()
        {
            cx.notify();
        }
    }

    pub(in crate::root) fn debounce_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.can_schedule_public_broadcaster_cost_estimate(kind, key) {
            return;
        }

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.cost_estimate_pending = true;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.replaced_estimate = None;
                    form.cost_estimate_pending = true;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            tokio::time::sleep(COST_ESTIMATE_DEBOUNCE).await;
            let _ = this.update(cx, |root, cx| {
                let current_id = match kind {
                    DeliveryFormKind::Send => {
                        root.send_forms.get(&key).map(|form| form.estimate_id)
                    }
                    DeliveryFormKind::Unshield => {
                        root.unshield_forms.get(&key).map(|form| form.estimate_id)
                    }
                };
                if current_id != Some(estimate_id) {
                    return;
                }
                match kind {
                    DeliveryFormKind::Send => {
                        root.estimate_send_public_broadcaster_cost_from_form(key, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.estimate_unshield_public_broadcaster_cost_from_form(key, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn can_schedule_public_broadcaster_cost_estimate(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> bool {
        // Debug UI fixture: a `delivery:` mode's form holds a stand-in state.
        #[cfg(debug_assertions)]
        {
            if crate::root::private_swap::ui_fixture_holds_delivery() {
                return false;
            }
        }
        match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).is_some_and(|form| {
                !form.generating && form.delivery_mode == DeliveryMode::PublicBroadcaster
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).is_some_and(|form| {
                !form.generating && form.delivery_mode == DeliveryMode::PublicBroadcaster
            }),
        }
    }

    pub(in crate::root) fn clear_pending_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                let changed = form.cost_estimate_pending || form.estimating_cost;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                changed
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                let changed = form.cost_estimate_pending || form.estimating_cost;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                changed
            }),
        };
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn reschedule_ready_public_broadcaster_cost_estimates(
        &mut self,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let send_keys = self
            .send_forms
            .iter()
            .filter_map(|(key, form)| {
                (key.chain_id == chain_id
                    && form.delivery_mode == DeliveryMode::PublicBroadcaster
                    && !form.generating
                    && public_broadcaster_estimate_needs_ready_retry(
                        form.cost_estimate.is_some()
                            || form.result.is_some()
                            || form.error.is_some(),
                        form.cost_estimate_pending || form.estimating_cost,
                    ))
                .then_some(*key)
            })
            .collect::<Vec<_>>();
        let unshield_keys = self
            .unshield_forms
            .iter()
            .filter_map(|(key, form)| {
                (key.chain_id == chain_id
                    && form.delivery_mode == DeliveryMode::PublicBroadcaster
                    && !form.generating
                    && public_broadcaster_estimate_needs_ready_retry(
                        form.cost_estimate.is_some()
                            || form.result.is_some()
                            || form.error.is_some(),
                        form.cost_estimate_pending || form.estimating_cost,
                    ))
                .then_some(*key)
            })
            .collect::<Vec<_>>();

        for key in send_keys {
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        }
        for key in unshield_keys {
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        }
    }
}

pub(in crate::root) const fn public_broadcaster_estimate_needs_ready_retry(
    has_completed_state: bool,
    in_flight: bool,
) -> bool {
    !has_completed_state && !in_flight
}
