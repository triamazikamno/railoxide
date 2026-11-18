use super::*;

pub(super) fn render_proposal_decoded_preview(
    preview: &ProposalDecodedPreview,
    identity: &ProposalIdentity,
    ordinal: usize,
) -> gpui::Div {
    let mut inset = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_sm()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_2()
                .child(app_strong_text(preview.verb.clone()).text_size(px(12.0)))
                .child(div().flex_1().min_w(px(0.0)))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(
                            Icon::new(RailgunActionIcon::Sparkles)
                                .with_size(px(13.0))
                                .text_color(rgb(theme::PRIMARY)),
                        )
                        .child(app_muted_text("DECODED").text_size(px(10.0))),
                ),
        );

    if let Some(hero) = &preview.hero {
        let mut amount = app_strong_text(hero.rounded.clone()).text_size(px(17.0));
        if hero.monospace {
            amount = amount.font_family(APP_MONO_FONT_FAMILY);
        }
        if hero.danger {
            amount = amount.text_color(rgb(theme::DANGER));
        }
        let mut amount_group = div()
            .min_w(px(0.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(amount.whitespace_normal());
        if let Some(copy_value) = &hero.amount_copy_value {
            amount_group = amount_group.child(
                div()
                    .id(proposal_action_id(
                        identity,
                        ordinal,
                        "decoded-hero-raw-amount",
                        "action",
                    ))
                    .flex_none()
                    .tooltip(|window, cx| Tooltip::new("Copy raw amount").build(window, cx))
                    .child(clipboard_with_toast(
                        proposal_action_id(
                            identity,
                            ordinal,
                            "decoded-hero-raw-amount",
                            "clipboard",
                        ),
                        copy_value.clone(),
                    )),
            );
        }
        let mut body = div()
            .min_w(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(amount_group);
        if let Some(context) = &hero.context {
            let mut context_group = div()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(app_muted_text(context.clone()).text_size(px(11.0)));
            if let Some(copy_value) = &hero.context_copy_value {
                context_group = context_group.child(
                    div()
                        .id(proposal_action_id(
                            identity,
                            ordinal,
                            "decoded-hero-token-address",
                            "action",
                        ))
                        .flex_none()
                        .tooltip(|window, cx| Tooltip::new("Copy token address").build(window, cx))
                        .child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                "decoded-hero-token-address",
                                "clipboard",
                            ),
                            copy_value.clone(),
                        )),
                );
            }
            body = body.child(context_group);
        }
        let mut hero_row = div().w_full().min_w(px(0.0)).flex().items_center().gap_3();
        if let Some(icon) = hero.icon.clone() {
            hero_row = hero_row.child(img(icon).size(px(30.0)).rounded_full().flex_none());
        }
        inset = inset.child(hero_row.child(body));
    }

    inset = inset.children(
        preview
            .details
            .iter()
            .enumerate()
            .map(|(detail_index, detail)| match detail {
                ProposalDecodedDetail::Connector => {
                    div().w_full().min_w(px(0.0)).flex().pl(px(66.0)).child(
                        Icon::new(IconName::ArrowDown)
                            .with_size(px(13.0))
                            .text_color(rgb(theme::TEXT_SUBTLE)),
                    )
                }
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    badge,
                } => {
                    let role_address_line = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_muted_text(role.clone())
                                .w(px(58.0))
                                .flex_none()
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(12.0))
                                .line_height(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        )
                        .child(
                            app_text(railgun_ui::short_address(address))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .font_weight(FontWeight::LIGHT)
                                .text_size(px(12.0))
                                .line_height(px(12.0)),
                        );
                    let copy_control = div()
                        .id(proposal_action_id(
                            identity,
                            ordinal,
                            &format!("decoded-party-{detail_index}"),
                            "action",
                        ))
                        .flex_none()
                        .tooltip({
                            let tooltip = format!("Copy {role}");
                            move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                &format!("decoded-party-{detail_index}"),
                                "clipboard",
                            ),
                            copy_value.clone(),
                        ));
                    let party_line = div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .child(role_address_line)
                        .child(copy_control);
                    let mut row = div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(party_line);
                    if let Some(badge) = badge {
                        row = row.child(proposal_address_badge(badge.clone()));
                    }
                    row
                }
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value,
                    monospace,
                } => {
                    let mut rendered_value = app_text(value.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .whitespace_normal();
                    if *monospace {
                        rendered_value = rendered_value.font_family(APP_MONO_FONT_FAMILY);
                    }
                    let mut row = div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            app_muted_text(role.clone())
                                .w(px(58.0))
                                .flex_none()
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(12.0))
                                .line_height(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        )
                        .child(rendered_value);
                    if let Some(copy_value) = copy_value {
                        row = row.child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                &format!("decoded-value-{detail_index}"),
                                "clipboard",
                            ),
                            copy_value.clone(),
                        ));
                    }
                    row
                }
            }),
    );
    if let Some(warning) = &preview.unlimited_warning {
        inset = inset.child(
            Alert::error(
                proposal_action_id(identity, ordinal, "decoded-unlimited", "alert"),
                warning.clone(),
            )
            .small(),
        );
    }
    inset
}
