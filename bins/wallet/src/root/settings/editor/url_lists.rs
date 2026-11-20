use super::*;

impl WalletSettingsEditor {
    pub(in crate::root) fn open_settings_url_dialog(
        &self,
        kind: &SettingsUrlListKind,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let initial_value = index
            .and_then(|index| kind.endpoints(&self.draft).get(index).cloned())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial_value));
        let title = kind.dialog_title(index.is_some());
        let help = kind.dialog_help();
        let action_label = SharedString::from(if index.is_some() { "Save" } else { "Add" });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(560.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_input = input.clone();
        let save_kind = kind.clone();
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_input = dialog_input.clone();
            let save_kind = save_kind.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title.clone()))
                .footer(dialog_footer(action_label.clone(), true))
                .on_ok(move |_event, _window, cx| {
                    let value = save_input.read(cx).value().trim().to_string();
                    let save_kind = save_kind.clone();
                    save_editor.update(cx, |editor, cx| {
                        match index {
                            Some(index) => save_kind.set_endpoint(&mut editor.draft, index, &value),
                            None => save_kind.add_endpoint(&mut editor.draft, &value),
                        }
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(render_settings_url_dialog_content(
                    &dialog_input,
                    content_width,
                    help,
                ))
        });
        let focus_input = input;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    pub(in crate::root) fn render_settings_url_list(
        editor: &Entity<Self>,
        kind: &SettingsUrlListKind,
        endpoints: Vec<String>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let add_kind = kind.clone();
        let body =
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().flex().justify_end().child(
                    settings_icon_button(kind.add_id(), IconName::Plus, "Add").on_click(
                        move |_event, window, cx| {
                            let kind = add_kind.clone();
                            add_editor.update(cx, |editor, cx| {
                                editor.open_settings_url_dialog(&kind, None, window, cx);
                            });
                        },
                    ),
                ));

        let endpoint_count = endpoints.len();
        let mut list = div().w_full().flex().flex_col();
        if endpoints.is_empty() {
            list = list.child(app_muted_text(kind.empty_text()).py(px(8.0)));
        }
        for (index, endpoint) in endpoints.into_iter().enumerate() {
            let edit_editor = editor.clone();
            let edit_kind = kind.clone();
            let remove_editor = editor.clone();
            let remove_kind = kind.clone();
            list = list.child(
                settings_list_row(kind.row_id(index), index + 1 == endpoint_count)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(px(13.0))
                            .line_height(px(18.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(endpoint)),
                    )
                    .child(
                        settings_row_actions()
                            .child(
                                settings_icon_button(
                                    kind.edit_id(index),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let kind = edit_kind.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_settings_url_dialog(
                                                &kind,
                                                Some(index),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_danger_icon_button(
                                    kind.remove_id(index),
                                    Icon::new(RailgunActionIcon::Trash2),
                                    "Remove",
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        let kind = remove_kind.clone();
                                        remove_editor.update(cx, |editor, cx| {
                                            kind.remove_endpoint(&mut editor.draft, index);
                                            editor.programmatic_draft_changed(cx);
                                        });
                                    },
                                ),
                            ),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn settings_url_list_item(
        title: impl Into<SharedString>,
        editor: Entity<Self>,
        kind: SettingsUrlListKind,
        endpoints: Vec<String>,
    ) -> SettingItem {
        SettingItem::new(
            title,
            SettingField::<SharedString>::render(move |_options, _window, _cx| {
                Self::render_settings_url_list(&editor, &kind, endpoints.clone())
            }),
        )
        .layout(Axis::Vertical)
    }

    pub(in crate::root) fn open_waku_peer_dialog(
        &self,
        kind: WakuPeerList,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let initial = index
            .and_then(|index| kind.peers(&self.draft).get(index).cloned())
            .unwrap_or_default();
        let inputs = WakuDirectPeerDialogInputs {
            peer_id: cx.new(|cx| InputState::new(window, cx).default_value(initial.peer_id)),
            addr: cx.new(|cx| InputState::new(window, cx).default_value(initial.addr)),
        };
        let title = match (kind, index.is_some()) {
            (WakuPeerList::Direct, true) => "Edit direct peer",
            (WakuPeerList::Direct, false) => "Add direct peer",
            (WakuPeerList::Backup, true) => "Edit backup peer",
            (WakuPeerList::Backup, false) => "Add backup peer",
        };
        let action_label = SharedString::from(if index.is_some() { "Save" } else { "Add" });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_inputs = inputs.clone();
        let save_inputs = inputs.clone();
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title))
                .footer(dialog_footer(action_label.clone(), true))
                .on_ok(move |_event, _window, cx| {
                    let peer = waku_direct_peer_from_dialog_inputs(&save_inputs, cx);
                    save_editor.update(cx, |editor, cx| {
                        match index {
                            Some(index) => set_waku_peer(&mut editor.draft, kind, index, peer),
                            None => add_waku_peer(&mut editor.draft, kind, peer),
                        }
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(render_waku_direct_peer_dialog_content(
                    &dialog_inputs,
                    content_width,
                ))
        });
        let focus_input = inputs.peer_id;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    pub(in crate::root) fn render_waku_peer_list(
        editor: &Entity<Self>,
        kind: WakuPeerList,
        peers: Vec<WakuDirectPeerSetting>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let kind_id = kind.id();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().justify_end().child(
                settings_icon_button(
                    SharedString::from(format!("wallet-settings-waku-{kind_id}-peer-add")),
                    IconName::Plus,
                    "Add",
                )
                .on_click(move |_event, window, cx| {
                    add_editor.update(cx, |editor, cx| {
                        editor.open_waku_peer_dialog(kind, None, window, cx);
                    });
                }),
            ),
        );

        let peer_count = peers.len();
        let mut list = div().w_full().flex().flex_col();
        if peers.is_empty() {
            let message = match kind {
                WakuPeerList::Direct => "No additional direct peers configured.",
                WakuPeerList::Backup => "No backup peers configured.",
            };
            list = list.child(app_muted_text(message).py_2());
        }
        for (index, peer) in peers.into_iter().enumerate() {
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let row_id =
                SharedString::from(format!("wallet-settings-waku-{kind_id}-peer-row-{index}"));
            list = list.child(
                settings_list_row(row_id, index + 1 == peer_count)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(13.0))
                                    .line_height(px(18.0))
                                    .text_color(rgb(theme::TEXT))
                                    .child(SharedString::from(peer.peer_id)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0))
                                    .line_height(px(16.0))
                                    .text_color(rgb(theme::TEXT_SUBTLE))
                                    .child(SharedString::from(peer.addr)),
                            ),
                    )
                    .child(
                        settings_row_actions()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-waku-{kind_id}-peer-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_waku_peer_dialog(
                                                kind,
                                                Some(index),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_danger_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-waku-{kind_id}-peer-remove-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Trash2),
                                    "Remove",
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        remove_editor.update(cx, |editor, cx| {
                                            remove_waku_peer(&mut editor.draft, kind, index);
                                            editor.programmatic_draft_changed(cx);
                                        });
                                    },
                                ),
                            ),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn waku_peer_list_item(
        editor: Entity<Self>,
        kind: WakuPeerList,
        peers: Vec<WakuDirectPeerSetting>,
    ) -> SettingItem {
        let (title, description) = match kind {
            WakuPeerList::Direct => (
                "Direct peers",
                "Additional peers available immediately, using the selected network mode.",
            ),
            WakuPeerList::Backup => (
                "Backup peers",
                "Added after 60 seconds without a connection, then reconnected normally. Discovery can find these peers earlier.",
            ),
        };
        SettingItem::new(
            title,
            SettingField::<SharedString>::render(move |_options, _window, _cx| {
                Self::render_waku_peer_list(&editor, kind, peers.clone())
            }),
        )
        .description(description)
        .layout(Axis::Vertical)
    }
}
