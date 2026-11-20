#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    // One header renderer keeps the private actions in one row and one order.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_private_balance_hero(
        root: Entity<Self>,
        total_balance: String,
        receive_available: bool,
        send_asset: Option<UnshieldAsset>,
        unshield_asset: Option<UnshieldAsset>,
        swap_entry: Option<Result<Address, &'static str>>,
        actions_available: bool,
        syncing: bool,
    ) -> gpui::Div {
        let receive_root = root.clone();
        let send_root = root.clone();
        let swap_root = root.clone();
        let unshield_root = root;
        let can_send = actions_available && send_asset.is_some();
        let can_unshield = actions_available && unshield_asset.is_some();

        ui::private_assets::private_balance(
            total_balance,
            false,
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Self::render_private_receive_button(
                    receive_root,
                    "wallet-private-hero-receive",
                    receive_available,
                ))
                .child(
                    app_button("wallet-private-hero-send", "Send")
                        .child(Icon::new(RailgunActionIcon::Send).small())
                        .outline()
                        .when(can_send, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_send)
                        .tooltip(private_send_action_tooltip(
                            can_send,
                            actions_available,
                            syncing,
                            "No sendable private asset available",
                        ))
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = send_asset.clone() else {
                                return;
                            };
                            send_root.update(cx, |root, cx| {
                                root.open_send_form(asset, window, cx);
                            });
                        }),
                )
                .child(
                    app_button("wallet-private-hero-unshield", "Unshield")
                        .child(Icon::new(IconName::Globe).small())
                        .outline()
                        .when(can_unshield, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_unshield)
                        .tooltip(private_unshield_action_tooltip(
                            can_unshield,
                            actions_available,
                            syncing,
                            "No unshieldable private asset available",
                        ))
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = unshield_asset.clone() else {
                                return;
                            };
                            unshield_root.update(cx, |root, cx| {
                                root.open_unshield_form(asset, window, cx);
                            });
                        }),
                )
                .when_some(swap_entry, |actions, entry| {
                    actions.child(render_private_swap_button(
                        "wallet-private-hero-swap".into(),
                        entry,
                        "Swap a private token for another, privately",
                        swap_root,
                    ))
                }),
        )
    }

    pub(super) fn render_private_asset_row(
        root: Entity<Self>,
        ix: usize,
        asset: FormattedTokenTotal,
        snapshot: &ListUtxosOutput,
        swap_profile: Option<wallet_ops::settings::SwapProfile>,
        actions_available: bool,
        syncing: bool,
    ) -> gpui::Div {
        let send_asset = build_send_asset(snapshot, &asset);
        let can_send = actions_available && send_asset.is_some();
        let unshield_asset = build_unshield_asset(snapshot, &asset);
        let can_unshield = actions_available && unshield_asset.is_some();
        let send_tooltip = private_send_action_tooltip(
            can_send,
            actions_available,
            syncing,
            "No spendable private balance for this token",
        );
        let unshield_tooltip = private_unshield_action_tooltip(
            can_unshield,
            actions_available,
            syncing,
            "No unshieldable private balance for this token",
        );
        let send_opacity = if can_send { 1.0 } else { 0.5 };
        let unshield_opacity = if can_unshield { 1.0 } else { 0.5 };
        let show_pending_poi = should_show_pending_poi_amount(asset.pending_poi_total);
        let pending_poi_amount = asset.pending_poi_amount.clone();
        let show_pending_incoming = should_show_pending_amount(asset.pending_incoming_total);
        let show_pending_outgoing = should_show_pending_amount(asset.pending_outgoing_total);
        let pending_incoming_amount = asset.pending_incoming_amount.clone();
        let pending_outgoing_amount = asset.pending_outgoing_amount.clone();
        let asset_label = asset.label.clone();
        let (primary_amount, secondary_amount) = private_asset_display_amounts(&asset);
        let swap_entry = swap_entry_availability(
            swap_profile.as_ref(),
            asset.token,
            unshield_asset.is_some(),
            actions_available,
        )
        .map(|availability| availability.map(|()| asset.token.unwrap_or_default()));
        let row_group = SharedString::from(format!("wallet-private-asset-row-{ix}"));
        let send_root = root.clone();
        let swap_root = root.clone();
        let unshield_root = root;

        ui::private_assets::PrivateAssetRow {
            compact: false,
            label: asset_label.into(),
            icon: asset.icon_path.map(Into::into),
            primary: primary_amount.into(),
            secondary: secondary_amount.map(Into::into),
            pending_verification: show_pending_poi.then(|| pending_poi_amount.into()),
            pending_incoming: show_pending_incoming.then(|| pending_incoming_amount.into()),
            pending_outgoing: show_pending_outgoing.then(|| pending_outgoing_amount.into()),
            actions: Some(
                (div()
                    .group(row_group.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .opacity(0.0)
                    .group_hover(row_group, |this| this.opacity(1.0))
                    .hover(|this| this.opacity(1.0))
                    .child(
                        app_button(
                            SharedString::from(format!("wallet-asset-send-{ix}")),
                            "Send",
                        )
                        .child(Icon::new(RailgunActionIcon::Send).small())
                        .outline()
                        .when(can_send, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_send)
                        .opacity(send_opacity)
                        .tooltip(send_tooltip)
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = send_asset.clone() else {
                                return;
                            };
                            send_root.update(cx, |root, cx| {
                                root.open_send_form(asset, window, cx);
                            });
                        }),
                    )
                    .child(
                        app_button(
                            SharedString::from(format!("wallet-asset-unshield-{ix}")),
                            "Unshield",
                        )
                        .child(Icon::new(IconName::Globe).small())
                        .outline()
                        .when(can_unshield, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_unshield)
                        .opacity(unshield_opacity)
                        .tooltip(unshield_tooltip)
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = unshield_asset.clone() else {
                                return;
                            };
                            unshield_root.update(cx, |root, cx| {
                                root.open_unshield_form(asset, window, cx);
                            });
                        }),
                    )
                    .when_some(swap_entry, |actions, entry| {
                        actions.child(
                            render_private_swap_button(
                                SharedString::from(format!("wallet-asset-swap-{ix}")),
                                entry,
                                "Swap this token privately",
                                swap_root,
                            )
                            .opacity(if entry.is_ok() {
                                1.0
                            } else {
                                0.5
                            }),
                        )
                    }))
                .into_any_element(),
            ),
        }
        .into_div()
        .group(SharedString::from(format!("wallet-private-asset-row-{ix}")))
    }
}

/// Swap sits beside Receive, Send and Unshield with the same outline treatment. A disabled
/// Swap keeps its reason in the tooltip.
fn render_private_swap_button(
    id: SharedString,
    entry: Result<Address, &'static str>,
    ready_tooltip: &'static str,
    root: Entity<WalletRoot>,
) -> gpui_component::button::Button {
    let enabled = entry.is_ok();
    app_button(id, "Swap")
        .child(
            Icon::empty()
                .path(ui::icons::arrow_right_left_icon_path())
                .small(),
        )
        .outline()
        .when(enabled, |button| button.bg(rgb(theme::SURFACE)))
        .disabled(!enabled)
        .tooltip(entry.err().unwrap_or(ready_tooltip))
        .on_click(move |_event, window, cx| {
            if let Ok(token) = entry {
                root.update(cx, |root, cx| {
                    root.open_private_swap_form(token, window, cx);
                });
            }
        })
}
