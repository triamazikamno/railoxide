use gpui::{
    ElementId, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::{
    Icon, Sizable,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use wallet_ops::vault::{PublicAccountMetadata, PublicAccountSource, PublicAccountStatus};

use crate::assets::RailgunPublicAccountIcon;
use crate::root::walletconnect::walletconnect_logo_with_presence;

pub(super) fn public_account_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    let tooltip: SharedString = tooltip.into();
    Button::new(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .compact()
        .accessibility_label(tooltip.clone())
        .tooltip(tooltip)
}

pub(super) fn public_account_walletconnect_button(
    id: impl Into<ElementId>,
    has_active_session: bool,
) -> Button {
    Button::new(id)
        .text()
        .xsmall()
        .compact()
        .cursor_pointer()
        .accessibility_label(if has_active_session {
            "Manage WalletConnect sessions"
        } else {
            "Connect dapp with WalletConnect"
        })
        .tooltip(if has_active_session {
            "Manage WalletConnect sessions"
        } else {
            "Connect dapp with WalletConnect"
        })
        .child(walletconnect_logo_with_presence(
            px(16.0),
            has_active_session,
        ))
}

pub(super) fn public_account_metadata_badge(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> impl IntoElement {
    let tooltip = tooltip.into();
    div()
        .id(id)
        .flex()
        .size(px(18.0))
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(rgb(ui::theme::SURFACE))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(
            Icon::new(icon)
                .xsmall()
                .text_color(rgb(ui::theme::TEXT_MUTED)),
        )
}

pub(super) const fn public_account_status_id(status: PublicAccountStatus) -> &'static str {
    match status {
        PublicAccountStatus::Active => "active",
        PublicAccountStatus::Inactive => "inactive",
    }
}

pub(in crate::root) fn public_account_matches_search(
    account: &PublicAccountMetadata,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    account
        .label
        .as_deref()
        .is_some_and(|label| label.to_ascii_lowercase().contains(&query))
        || format!("{:#x}", account.address).contains(&query)
}

pub(in crate::root) fn public_account_display_label(
    account: &PublicAccountMetadata,
) -> Option<String> {
    account
        .label
        .as_ref()
        .filter(|label| !label.trim().is_empty())
        .cloned()
}

pub(in crate::root) fn next_public_account_label_number(account_count: usize) -> u32 {
    u32::try_from(account_count)
        .ok()
        .and_then(|count| count.checked_add(1))
        .unwrap_or(u32::MAX)
}

pub(in crate::root) const fn public_account_source_label(
    source: PublicAccountSource,
) -> &'static str {
    match source {
        PublicAccountSource::Derived => "Derived",
        PublicAccountSource::HardwareDerived => "Hardware",
        PublicAccountSource::Imported => "Imported",
        PublicAccountSource::ExecutorDerived(_) => "Stealth",
    }
}

pub(in crate::root) const fn public_account_source_icon(
    source: PublicAccountSource,
) -> RailgunPublicAccountIcon {
    match source {
        PublicAccountSource::Derived
        | PublicAccountSource::HardwareDerived
        | PublicAccountSource::ExecutorDerived(_) => RailgunPublicAccountIcon::Derived,
        PublicAccountSource::Imported => RailgunPublicAccountIcon::Imported,
    }
}
