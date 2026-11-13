use super::*;
use crate::root::actions::{PRIVATE_ACTION_FORM_KEY_CONTEXT, RefreshBroadcasterEstimate};
use crate::root::{ConfirmationDialogProps, confirmation_dialog};
use std::rc::Rc;
use ui::dialog::WalletDialogExt as _;

mod self_broadcast_gas;
mod send;
mod unshield;

const DOUBLE_PAYMENT_RISK: ConfirmationDialogProps = ConfirmationDialogProps::danger(
    "Earlier proof may still go through",
    "An earlier proof for this payment may still be executed by the broadcaster. Continuing may use different notes, so if that proof also goes through, the recipient could be paid twice.",
    None,
    "Continue anyway",
);

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

    pub(in crate::root) fn open_private_action_dialog(
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        title_action: &'static str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog_width = (window.viewport_size().width * 0.92).min(PRIVATE_ASSET_LIST_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let max_height = window.viewport_size().height * 0.88;
            let close_root = root.clone();
            let content_root = root.clone();
            let refresh_root = root.clone();
            root.update(cx, |root, cx| {
                root.sync_private_action_asset_select_for_dialog(kind, key, window, cx);
            });
            let content = content_root.read(cx);
            let (asset_label, icon_path) =
                content.private_action_dialog_asset(kind, key).map_or_else(
                    || ("asset".to_string(), None),
                    |asset| (asset.label.clone(), asset.icon_path.clone()),
                );
            let (asset_select, asset_select_disabled) =
                content.private_action_dialog_asset_select(kind, key);
            let child = match kind {
                DeliveryFormKind::Send => {
                    content.render_send_form(content_root.clone(), key, content_width)
                }
                DeliveryFormKind::Unshield => {
                    content.render_unshield_form(content_root.clone(), key, content_width)
                }
            };
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(max_height)
                .title(private_action_title_row(
                    title_action,
                    &asset_label,
                    icon_path,
                    asset_select,
                    asset_select_disabled,
                ))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| match kind {
                        DeliveryFormKind::Send => root.close_send_form(key, cx),
                        DeliveryFormKind::Unshield => root.close_unshield_form(key, cx),
                    });
                })
                .child(
                    div()
                        .key_context(PRIVATE_ACTION_FORM_KEY_CONTEXT)
                        .on_action(move |_: &RefreshBroadcasterEstimate, _, cx| {
                            refresh_root.update(cx, |root, cx| {
                                let can_refresh = match kind {
                                    DeliveryFormKind::Send => {
                                        root.send_forms.get(&key).is_some_and(|form| {
                                            form.cost_estimate.is_some()
                                                && !form.estimating_cost
                                                && should_render_public_broadcaster_cost_preview(
                                                    form.delivery_mode,
                                                    form.result.is_some(),
                                                    form.error.is_some(),
                                                )
                                        })
                                    }
                                    DeliveryFormKind::Unshield => {
                                        root.unshield_forms.get(&key).is_some_and(|form| {
                                            form.cost_estimate.is_some()
                                                && !form.estimating_cost
                                                && should_render_public_broadcaster_cost_preview(
                                                    form.delivery_mode,
                                                    form.result.is_some(),
                                                    form.error.is_some(),
                                                )
                                        })
                                    }
                                };
                                if can_refresh {
                                    root.reroll_public_broadcaster_cost_estimate(kind, key, cx);
                                }
                            });
                        })
                        .when_some(content.private_action_form.as_ref(), |this, form| {
                            this.track_focus(&form.focus)
                        })
                        .child(child),
                )
        });
    }

    pub(in crate::root) fn private_action_dialog_asset(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<&UnshieldAsset> {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map(|form| &form.asset),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| &form.asset),
        }
    }

    pub(in crate::root) fn private_action_dialog_asset_select(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> (
        Option<Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>>,
        bool,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return (None, false);
                };
                let show = self
                    .private_action_asset_options(kind, form.asset.chain_id)
                    .len()
                    > 1;
                let disabled = form.generating || send_form_submitted(form);
                (show.then(|| form.asset_select.clone()), disabled)
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return (None, false);
                };
                let show = self
                    .private_action_asset_options(kind, form.asset.chain_id)
                    .len()
                    > 1;
                let disabled = form.generating || unshield_form_submitted(form);
                (show.then(|| form.asset_select.clone()), disabled)
            }
        }
    }

    pub(in crate::root) fn consume_programmatic_amount_input_change(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
        }
    }

    pub(in crate::root) fn set_private_action_amount_to_max(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let amount = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return;
                };
                send_form_max_entered_amount(form, form.delivery_mode, form.fee_mode)
                    .unwrap_or(form.asset.max_batched)
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return;
                };
                unshield_max_fill_amount(
                    unshield_form_max_entered_amount(form, form.delivery_mode, form.fee_mode),
                    form.asset.max_batched,
                    form.fee_mode,
                )
            }
        };
        let changed = self.set_programmatic_amount_input(kind, key, amount, window, cx);
        if changed {
            if kind == DeliveryFormKind::Unshield {
                self.refresh_unshield_native_top_up_state(key, cx);
            }
            self.schedule_public_broadcaster_cost_estimate(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn set_programmatic_amount_input(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_send_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_unshield_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
        }
    }

    pub(in crate::root) fn set_private_action_asset(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_asset(key, asset, window, cx),
            DeliveryFormKind::Unshield => self.set_unshield_asset(key, asset, window, cx),
        }
    }

    pub(in crate::root) fn set_private_action_asset_token(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(asset) = self
            .private_action_asset_options(kind, key.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)
        else {
            return;
        };
        self.set_private_action_asset(kind, key, asset, window, cx);
    }

    pub(in crate::root) fn set_favorites_only_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_favorites_only_broadcasters(key, enabled, cx),
            DeliveryFormKind::Unshield => {
                self.set_unshield_favorites_only_broadcasters(key, enabled, cx);
            }
        }
    }

    pub(in crate::root) fn set_allow_suspicious_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        allow: bool,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_allow_suspicious_broadcasters(key, allow, cx),
            DeliveryFormKind::Unshield => {
                self.set_unshield_allow_suspicious_broadcasters(key, allow, cx);
            }
        }
    }

    pub(in crate::root) fn set_transaction_fee_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.transaction_fee_breakdown_open == open {
                    false
                } else {
                    form.transaction_fee_breakdown_open = open;
                    true
                }
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.transaction_fee_breakdown_open == open {
                    false
                } else {
                    form.transaction_fee_breakdown_open = open;
                    true
                }
            }),
        };
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn set_sponsored_funding_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_funding_breakdown_open == open {
                    false
                } else {
                    form.sponsored_funding_breakdown_open = open;
                    true
                }
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_funding_breakdown_open == open {
                    false
                } else {
                    form.sponsored_funding_breakdown_open = open;
                    true
                }
            }),
        };
        if changed {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn pinned_form_keeps_its_pin_until_the_user_accepts_leaving_it(cx: &mut gpui::TestAppContext) {
        let path = std::env::temp_dir().join(format!(
            "pinned-private-form-{:032x}",
            rand::random::<u128>()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        cx.update(gpui_component::init);
        let mut root = None;
        let (host, cx) = cx.add_window_view(|window, cx| {
            let wallet =
                crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
            root = Some(wallet.clone());
            gpui_component::Root::new(wallet, window, cx)
        });
        let root = root.unwrap();
        let asset = UnshieldAsset {
            chain_id: 1,
            token: Address::repeat_byte(0x55),
            label: "TEST".into(),
            decimals: Some(18),
            total: U256::from(1_000),
            poi_verified_total: U256::from(1_000),
            max_batched: U256::from(1_000),
            icon_path: None,
        };
        let key = UnshieldAssetKey::from_asset(&asset);
        let pin = wallet_ops::PublicBroadcasterInputPin {
            inputs: vec![wallet_ops::SelectedInputIdentity {
                tree: 0,
                position: 7,
            }],
            overridden: false,
        };
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.initialize_send_form(asset, window, cx);
                root.send_forms.get_mut(&key).unwrap().input_pin = Some(pin.clone());
                let amount = root.send_forms[&key].amount_input.clone();
                amount.update(cx, |input, cx| input.set_value("0.5", window, cx));
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
                root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
                assert!(window.has_active_dialog(cx), "leaving the pin asks first");
                assert_eq!(
                    root.send_forms[&key].delivery_mode,
                    DeliveryMode::PublicBroadcaster
                );
            });
            // Cancel leaves the form as it was.
            window.close_all_dialogs(cx);
            root.update(cx, |root, cx| {
                assert_eq!(
                    root.send_forms[&key].delivery_mode,
                    DeliveryMode::PublicBroadcaster
                );
                assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
                root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        cx.run_until_parked();
        root.read_with(cx, |root, _| {
            let form = &root.send_forms[&key];
            assert_eq!(form.delivery_mode, DeliveryMode::ManualCalldata);
            assert_eq!(form.input_pin, None, "accepting the risk clears the pin");
        });
        cx.update(|window, _| window.remove_window());
        drop(host);
        drop(root);
        cx.run_until_parked();
        drop(entered);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }
}
