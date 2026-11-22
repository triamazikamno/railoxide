#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
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
                    content.render_unshield_form(content_root.clone(), key, content_width, cx)
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
}
