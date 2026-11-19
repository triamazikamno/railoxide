use super::*;

impl WalletRoot {
    pub(super) fn render_gateway_paired_browsers(
        &self,
        root: &Entity<Self>,
        popover: &Entity<PopoverState>,
        disabled: bool,
    ) -> impl IntoElement {
        let pair_root = root.clone();
        let pair_popover = popover.clone();
        let mut peers_content = gateway_section_content().gap_0();
        if self.gateway.snapshot.peers.is_empty() {
            peers_content = peers_content.child(
                app_muted_text(
                    "No browsers paired yet. Install the extension, then create a pairing.",
                )
                .text_size(px(12.0))
                .line_height(px(17.0)),
            );
        }
        let now = crate::root::utxo::now_epoch_secs();
        let peer_count = self.gateway.snapshot.peers.len();
        for (index, peer) in self.gateway.snapshot.peers.iter().enumerate() {
            let identity = alloy::hex::encode(peer.id.to_bytes());
            let identity_text = SharedString::from(identity.clone());
            let label = peer.label.clone().map(SharedString::from);
            let is_hex_identity = label.is_none();
            let display_name =
                label.unwrap_or_else(|| SharedString::from(short_identity(&identity)));
            let tooltip = identity_text.clone();
            let (status, color) = if peer.connected_sessions > 0 {
                ("Active", theme::SUCCESS)
            } else {
                ("Inactive", theme::TEXT_MUTED)
            };
            let last_active = gateway_peer_age(peer.last_active_at, now);
            let paired = gateway_peer_age(peer.paired_at, now);
            let mut detail = format!("Last active {last_active} · paired {paired}");
            if let Some(reported) = peer.extension_version.as_deref() {
                detail.push_str(" · v");
                detail.push_str(reported);
            }
            let update = peer.extension_version.as_deref().and_then(|reported| {
                let bundle = install_bundle();
                extension_update_available(reported, bundle).then_some((reported, bundle.version))
            });
            let update_root = root.clone();
            let update_popover = popover.clone();
            let menu_target = GatewayPeerMenuTarget {
                root: root.clone(),
                peer_id: peer.id,
                identity: identity_text,
                disabled,
                allow_unlock: peer.allow_unlock,
            };
            let revoke_root = root.clone();
            let peer_id = peer.id;
            peers_content =
                peers_content.child(
                    div()
                        .id(SharedString::from(format!("gateway-peer-row-{identity}")))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .py_1p5()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    app_text(display_name)
                                        .id(SharedString::from(format!(
                                            "gateway-peer-label-{identity}"
                                        )))
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.0))
                                        .line_height(px(17.0))
                                        .when(is_hex_identity, |this| {
                                            this.font_family(theme::APP_MONO_FONT_FAMILY)
                                        })
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(tooltip.clone()).build(window, cx)
                                        }),
                                )
                                .when_some(update, |this, (reported, latest)| {
                                    this.child(
                                    Button::new(SharedString::from(format!(
                                        "gateway-peer-update-{identity}"
                                    )))
                                    .ghost()
                                    .small()
                                    .flex_none()
                                    .icon(
                                        Icon::empty()
                                            .path(icons::circle_fading_arrow_up_icon_path())
                                            .text_color(rgb(theme::WARNING)),
                                    )
                                    .accessibility_label("Update extension")
                                    .tooltip(format!(
                                        "Update available: v{reported} installed, v{latest} bundled"
                                    ))
                                    .disabled(
                                        disabled || self.gateway.snapshot.listener_addr.is_none(),
                                    )
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        update_popover
                                            .update(cx, |state, cx| state.dismiss(window, cx));
                                        update_root.update(cx, |root, cx| {
                                            root.open_gateway_install_dialog(window, cx);
                                        });
                                    }),
                                )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .flex_none()
                                        .child(app_status_tag(status, color)),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "gateway-peer-revoke-{identity}"
                                    )))
                                    .ghost()
                                    .small()
                                    .flex_none()
                                    .icon(Icon::empty().path(icons::ban_icon_path()))
                                    .accessibility_label("Revoke pairing")
                                    .tooltip("Revoke pairing")
                                    .disabled(disabled)
                                    .on_click(
                                        move |_, window, cx| {
                                            revoke_gateway_peer(&revoke_root, peer_id, window, cx);
                                        },
                                    ),
                                ),
                        )
                        .child(
                            app_muted_text(detail)
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .line_height(px(17.0)),
                        )
                        .context_menu(move |menu, _window, cx| {
                            gateway_peer_menu(menu, &menu_target, cx)
                        }),
                );
            if index + 1 < peer_count {
                peers_content = peers_content.child(Separator::horizontal());
            }
        }
        let peers_toggle_root = root.clone();
        Collapsible::new()
            .open(self.gateway.paired_browsers_open)
            .w_full()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button_base("gateway-paired-browsers-toggle")
                            .text()
                            .small()
                            .compact()
                            .flex_1()
                            .min_w_0()
                            .accessibility_label("Paired browsers")
                            .icon(if self.gateway.paired_browsers_open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        app_button_label("Paired browsers")
                                            .text_color(rgb(theme::TEXT))
                                            .font_weight(FontWeight::MEDIUM),
                                    )
                                    .when(peer_count > 0, |this| {
                                        this.child(
                                            app_button_label(format!("({peer_count})"))
                                                .text_color(rgb(theme::TEXT_MUTED)),
                                        )
                                    }),
                            )
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                peers_toggle_root.update(cx, |root, cx| {
                                    root.gateway.paired_browsers_open =
                                        !root.gateway.paired_browsers_open;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("gateway-pair")
                            .ghost()
                            .small()
                            .flex_none()
                            .icon(IconName::Plus)
                            .accessibility_label("Create new pairing")
                            .tooltip("Create new pairing")
                            .disabled(disabled || self.gateway.snapshot.listener_addr.is_none())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                pair_popover.update(cx, |state, cx| state.dismiss(window, cx));
                                pair_root.update(cx, |root, cx| {
                                    root.open_gateway_pairing_dialog(window, cx);
                                });
                            }),
                    ),
            )
            .content(peers_content)
    }

    pub(super) fn render_gateway_dapp_permissions(
        &self,
        root: &Entity<Self>,
        disabled: bool,
    ) -> impl IntoElement {
        let mut permissions_content = gateway_section_content().gap_0();
        let permission_count = self.gateway.snapshot.permissions.len();
        for (index, permission) in self.gateway.snapshot.permissions.iter().enumerate() {
            let account = self
                .gateway
                .permission_accounts
                .get(&permission.public_account_uuid);
            let account_address = account.map(|account| account.address);
            let permission_id = permission.permission_id.clone();
            let url = SharedString::from(permission.url.clone());
            let display_url = SharedString::from(
                permission
                    .url
                    .strip_prefix("https://")
                    .or_else(|| permission.url.strip_prefix("http://"))
                    .unwrap_or(permission.url.as_str())
                    .to_owned(),
            );
            let menu_target = GatewayPermissionMenuTarget {
                root: root.clone(),
                permission_id: permission_id.clone(),
                url: url.clone(),
                address: account_address.map(|address| SharedString::from(format!("{address:#x}"))),
                peer_identity: SharedString::from(permission.paired_peer_id.clone()),
                disabled,
            };
            let revoke_root = root.clone();
            let mut account_row = gateway_permission_detail_row().gap_1();
            if let Some(label) = account.and_then(|account| account.label.clone()) {
                account_row =
                    account_row.child(div().min_w_0().truncate().child(SharedString::from(label)));
            }
            account_row = account_row.child(
                div()
                    .flex_none()
                    .font_family(theme::APP_MONO_FONT_FAMILY)
                    .child(SharedString::from(account_address.map_or_else(
                        || short_identity(&permission.public_account_uuid),
                        |address| short_address(&address),
                    ))),
            );
            let chain_label = chain_name(permission.chain_id)
                .map_or_else(|| format!("Chain {}", permission.chain_id), str::to_owned);
            let mut chain_row = gateway_permission_detail_row().gap_1p5();
            if let Some(path) = chain_icon_asset_path(permission.chain_id) {
                chain_row = chain_row.child(img(path).size(px(12.0)).flex_none());
            }
            chain_row = chain_row
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!("{chain_label} · browser"))),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme::APP_MONO_FONT_FAMILY)
                        .child(SharedString::from(short_identity(
                            &permission.paired_peer_id,
                        ))),
                );
            permissions_content = permissions_content.child(
                div()
                    .id(SharedString::from(format!(
                        "gateway-permission-row-{permission_id}"
                    )))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .min_w_0()
                    .py_1p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                app_text(display_url)
                                    .id(SharedString::from(format!(
                                        "gateway-permission-url-{permission_id}"
                                    )))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .line_height(px(17.0))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(url.clone()).build(window, cx)
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "gateway-permission-revoke-{permission_id}"
                                )))
                                .ghost()
                                .small()
                                .flex_none()
                                .icon(Icon::empty().path(icons::ban_icon_path()))
                                .accessibility_label("Revoke access")
                                .tooltip("Revoke access")
                                .disabled(disabled)
                                .on_click(
                                    move |_, window, cx| {
                                        revoke_gateway_permission(
                                            &revoke_root,
                                            permission_id.clone(),
                                            window,
                                            cx,
                                        );
                                    },
                                ),
                            ),
                    )
                    .child(account_row)
                    .child(chain_row)
                    .context_menu(move |menu, _window, cx| {
                        gateway_permission_menu(menu, &menu_target, cx)
                    }),
            );
            if index + 1 < permission_count {
                permissions_content = permissions_content.child(Separator::horizontal());
            }
        }
        let permissions_toggle_root = root.clone();
        Collapsible::new()
            .open(self.gateway.dapp_permissions_open)
            .w_full()
            .min_w_0()
            .gap_3()
            .child(
                app_button_base("gateway-dapp-permissions-toggle")
                    .text()
                    .small()
                    .compact()
                    .w_full()
                    .min_w_0()
                    .accessibility_label("Dapp permissions")
                    .icon(if self.gateway.dapp_permissions_open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                app_button_label("Dapp permissions")
                                    .text_color(rgb(theme::TEXT))
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .when(permission_count > 0, |this| {
                                this.child(
                                    app_button_label(format!("({permission_count})"))
                                        .text_color(rgb(theme::TEXT_MUTED)),
                                )
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        permissions_toggle_root.update(cx, |root, cx| {
                            root.gateway.dapp_permissions_open =
                                !root.gateway.dapp_permissions_open;
                            cx.notify();
                        });
                    }),
            )
            .content(permissions_content)
    }
}
