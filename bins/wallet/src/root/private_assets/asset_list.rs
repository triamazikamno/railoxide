#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn private_asset_presentation(
        &self,
        snapshot: &Arc<ListUtxosOutput>,
    ) -> Arc<PrivateAssetPresentation> {
        let rates_revision = *self
            .public_broadcaster_anchor_cache
            .subscribe_refreshes()
            .borrow();
        let mut cache = self.private_asset_presentation_cache.borrow_mut();
        if let Some(current) = &cache.current
            && current.wallet_generation == self.active_wallet_generation
            && Arc::ptr_eq(&current.snapshot, snapshot)
            && current.registry == self.effective_token_registry
            && current.rates_revision == rates_revision
        {
            return Arc::clone(&current.presentation);
        }
        let rows = format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        );
        let total = total_private_balance_usd_amount(&rows);
        let presentation = Arc::new(PrivateAssetPresentation { rows, total });
        cache.current = Some(CachedPrivateAssets {
            wallet_generation: self.active_wallet_generation,
            snapshot: Arc::clone(snapshot),
            registry: self.effective_token_registry.clone(),
            rates_revision,
            presentation: Arc::clone(&presentation),
        });
        presentation
    }

    pub(in crate::root) fn gateway_private_pending(
        &self,
        snapshot: &ListUtxosOutput,
        assets: &[FormattedTokenTotal],
    ) -> Option<wallet_ops::gateway::GatewayPrivatePending> {
        self.private_pending_summary_from_snapshot(snapshot, assets)
            .map(|summary| private_pending_presentation(&summary))
    }

    fn open_private_receive_address_dialog(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(address) = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
        else {
            return;
        };
        window.close_all_dialogs(cx);
        let dialog_width =
            (window.viewport_size().width * 0.92).min(PUBLIC_ADDRESS_QR_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let address_text = SharedString::from(address);
        let copy_id = SharedString::from("wallet-private-receive-address-copy");
        let root = cx.entity().downgrade();
        let generation = self.active_wallet_generation;
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Private receive address"))
                .child(render_public_address_qr_dialog_content(
                    None,
                    address_text.clone(),
                    None,
                    copy_id.clone(),
                    content_width,
                    {
                        let root = root.clone();
                        move |window, cx| {
                            let address = root.upgrade().and_then(|root| {
                                let root = root.read(cx);
                                (root.active_wallet_generation == generation)
                                    .then(|| root.view_session.as_ref()?.receive_address().ok())
                                    .flatten()
                            });
                            if let Some(address) = address {
                                ui::clipboard::copy_to_clipboard_with_toast(address, window, cx);
                            }
                        }
                    },
                ))
        });
    }

    pub(in crate::root) fn render_private_assets_body(
        &self,
        root: &Entity<Self>,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        if self.view_session.is_none() {
            return ui::private_assets::private_message("Choose a wallet to continue", None)
                .into_any_element();
        }
        match self.chain_states.get(&self.last_evm_chain) {
            Some(ChainUtxoState::Error { message, .. }) => self
                .render_chain_error_body(root, message.as_ref())
                .into_any_element(),
            Some(ChainUtxoState::Loading { progress }) => ui::private_assets::private_message_with(
                app_muted_shimmer_text(loading_summary(progress.latest())),
                None,
            )
            .into_any_element(),
            Some(
                state @ ChainUtxoState::Syncing {
                    snapshot, progress, ..
                },
            ) => self.render_private_asset_snapshot(
                root,
                snapshot,
                state.private_action_forms_available(),
                true,
                progress.latest(),
                cx,
            ),
            Some(state @ ChainUtxoState::Ready { snapshot, .. }) => self
                .render_private_asset_snapshot(
                    root,
                    snapshot,
                    state.private_action_forms_available(),
                    false,
                    None,
                    cx,
                ),
            Some(ChainUtxoState::Idle) | None => {
                ui::private_assets::private_message("Select a chain to load private balances", None)
                    .into_any_element()
            }
        }
    }

    fn render_private_asset_snapshot(
        &self,
        root: &Entity<Self>,
        snapshot: &Arc<ListUtxosOutput>,
        actions_available: bool,
        syncing: bool,
        progress: Option<wallet_ops::SyncProgressUpdate>,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let presentation = self.private_asset_presentation(snapshot);
        let assets = &presentation.rows;
        let total_balance = presentation.total.clone();
        let receive_available = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
            .is_some();
        let send_asset = assets
            .iter()
            .find_map(|asset| build_send_asset(snapshot, asset))
            .or_else(|| {
                let wrapped_native = self
                    .effective_chain_configs
                    .get(self.last_evm_chain)?
                    .wrapped_native_token?;
                crate::root::private_swap::ui_fixture_send_asset(
                    self.last_evm_chain,
                    wrapped_native,
                )
            });
        let unshield_asset = assets
            .iter()
            .find_map(|asset| build_unshield_asset(snapshot, asset))
            .or_else(|| {
                let wrapped_native = self
                    .effective_chain_configs
                    .get(self.last_evm_chain)?
                    .wrapped_native_token?;
                crate::root::private_swap::ui_fixture_unshield_asset(
                    self.last_evm_chain,
                    wrapped_native,
                )
            });
        let pending_summary = self.private_pending_summary_from_snapshot(snapshot, assets);
        let swap_profile = self.private_swap_profile();
        // Unfinished swaps share the pending-status slot, directly below the header.
        let swap_card = self
            .private_swaps_view()
            .filter(|view| view.read(cx).has_shown_swaps())
            .map(gpui::IntoElement::into_any_element);
        let fixture_swap_token = self
            .effective_chain_configs
            .get(self.last_evm_chain)
            .and_then(|chain| chain.wrapped_native_token)
            .and_then(crate::root::private_swap::ui_fixture_setup_swap_token);
        let swap_entry = swap_profile.as_ref().map(|profile| {
            assets
                .iter()
                .find_map(|asset| {
                    let spendable = build_unshield_asset(snapshot, asset).is_some();
                    (swap_entry_availability(Some(profile), asset.token, spendable, true)
                        == Some(Ok(())))
                    .then_some(asset.token)
                    .flatten()
                })
                .or(fixture_swap_token)
                .filter(|_| actions_available)
                .ok_or(if actions_available {
                    "No private token can be swapped"
                } else {
                    "Available after wallet session starts"
                })
        });
        // The `unshield`, `broadcaster-progress:`, `setup:` and `max:` UI fixtures offer a stand-in
        // asset without private funds.
        let stand_in =
            unshield_asset.is_some() || send_asset.is_some() || fixture_swap_token.is_some();
        let total_balance =
            total_balance.or_else(|| (assets.is_empty() && stand_in).then(|| "$0.00".to_owned()));
        if assets.is_empty() && !stand_in {
            let message = if syncing {
                app_muted_shimmer_text(loading_summary(progress))
            } else {
                app_muted_text("No private assets found")
            };
            if pending_summary.is_some() || swap_card.is_some() {
                let empty = if receive_available {
                    Self::render_private_empty_state(root.clone(), message, receive_available)
                        .into_any_element()
                } else {
                    ui::private_assets::private_message_with(message, None).into_any_element()
                };
                return div()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .py(px(16.0))
                    .child(
                        div()
                            .w(crate::root::PRIVATE_ASSET_LIST_WIDTH)
                            .max_w_full()
                            .mx_auto()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .children(swap_card)
                            .children(pending_summary.as_ref().map(|summary| {
                                Self::render_private_pending_status_card(root, summary)
                            })),
                    )
                    .child(div().flex_1().min_h(px(0.0)).child(empty))
                    .into_any_element();
            }
            if receive_available {
                return Self::render_private_empty_state(root.clone(), message, receive_available)
                    .into_any_element();
            }
            return ui::private_assets::private_message_with(message, None).into_any_element();
        }
        let has_total_balance = total_balance.is_some();

        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .child(
                div()
                    .w(crate::root::PRIVATE_ASSET_LIST_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .py(px(16.0))
                    .when_some(total_balance, |column, total_balance| {
                        column.child(Self::render_private_balance_hero(
                            root.clone(),
                            total_balance,
                            receive_available,
                            send_asset,
                            unshield_asset,
                            swap_entry,
                            actions_available,
                            syncing,
                        ))
                    })
                    .when(!has_total_balance && receive_available, |column| {
                        column.child(Self::render_private_receive_action(
                            root.clone(),
                            receive_available,
                        ))
                    })
                    .children(swap_card)
                    .when_some(pending_summary, |column, summary| {
                        column.child(Self::render_private_pending_status_card(root, &summary))
                    })
                    .children(assets.iter().cloned().enumerate().map(|(ix, asset)| {
                        Self::render_private_asset_row(
                            root.clone(),
                            ix,
                            asset,
                            snapshot,
                            swap_profile,
                            actions_available,
                            syncing,
                        )
                        .into_any_element()
                    })),
            )
            .into_any_element()
    }

    fn render_private_empty_state(
        root: Entity<Self>,
        message: gpui::Div,
        receive_available: bool,
    ) -> gpui::Div {
        ui::private_assets::private_message_with(
            message,
            Some(
                Self::render_private_receive_button(
                    root,
                    "wallet-private-empty-receive",
                    receive_available,
                )
                .into_any_element(),
            ),
        )
    }

    fn render_private_receive_action(root: Entity<Self>, receive_available: bool) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(Self::render_private_receive_button(
                root,
                "wallet-private-standalone-receive",
                receive_available,
            ))
    }

    pub(super) fn render_private_receive_button(
        root: Entity<Self>,
        id: &'static str,
        receive_available: bool,
    ) -> impl IntoElement {
        app_button(id, "Receive")
            .child(Icon::new(RailgunActionIcon::QrCode).small())
            .outline()
            .when(receive_available, |button| button.bg(rgb(theme::SURFACE)))
            .disabled(!receive_available)
            .tooltip("Show private receive address")
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    root.open_private_receive_address_dialog(window, cx);
                });
            })
    }
}
