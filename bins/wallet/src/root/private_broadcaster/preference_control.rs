#[allow(clippy::wildcard_imports)]
use super::*;

/// The preference indicator or control of a submission's broadcaster: Banned whenever it is
/// banned, the favorite state once it `submitted`, and Ban while the flow has `ban_offered`.
pub(in crate::root) const fn broadcaster_preference_control(
    banned: bool,
    favorite: bool,
    submitted: bool,
    ban_offered: bool,
) -> Option<BroadcasterPreferenceControl> {
    match (banned, submitted, favorite, ban_offered) {
        (true, ..) => Some(BroadcasterPreferenceControl::Banned),
        (false, true, true, _) => Some(BroadcasterPreferenceControl::Favorited),
        (false, true, false, _) => Some(BroadcasterPreferenceControl::AddFavorite),
        (false, false, _, true) => Some(BroadcasterPreferenceControl::Ban),
        (false, false, _, false) => None,
    }
}

/// Draw `control` for the broadcaster at `address`. Its buttons' IDs start with `id_prefix`,
/// and a click changes `root`'s preferences, then calls `on_change`.
pub(in crate::root) fn render_broadcaster_preference_control(
    control: BroadcasterPreferenceControl,
    id_prefix: SharedString,
    address: &str,
    root: gpui::WeakEntity<WalletRoot>,
    on_change: impl Fn(&mut App) + 'static,
) -> AnyElement {
    use ui::private_submission::OperationControl;

    let address = address.to_owned();
    match control {
        BroadcasterPreferenceControl::Banned => {
            render_broadcaster_preference_progress_chip("Banned", theme::DANGER, None)
                .into_any_element()
        }
        BroadcasterPreferenceControl::Favorited => render_broadcaster_preference_progress_chip(
            "Favorited",
            theme::WARNING,
            Some(IconName::Star),
        )
        .into_any_element(),
        BroadcasterPreferenceControl::AddFavorite => {
            app_button_base(SharedString::from(format!("{id_prefix}-favorite")))
                .outline()
                .xsmall()
                .flex_none()
                .icon(Icon::new(IconName::Star))
                .accessibility_label("Add to favorites")
                .tooltip(
                    "Save this broadcaster to your favorites so future transactions can prefer it.",
                )
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.add_favorite_broadcaster(&address, cx);
                    });
                    on_change(cx);
                })
                .into_any_element()
        }
        BroadcasterPreferenceControl::Ban => ui::private_submission::operation_controls(
            id_prefix,
            [OperationControl::Ban],
            move |_control, _, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.add_banned_broadcaster(&address, cx);
                });
                on_change(cx);
            },
        )
        .flex_none()
        .into_any_element(),
    }
}

pub(in crate::root) fn render_broadcaster_preference_progress_chip(
    label: &'static str,
    color: u32,
    icon: Option<IconName>,
) -> impl IntoElement {
    app_status_tag(label, color)
        .flex_none()
        .gap_1()
        .children(icon.map(|icon| Icon::new(icon).size(rems(0.8125))))
}
