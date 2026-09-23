use gpui::{ElementId, SharedString};
use gpui_component::{
    Icon, Sizable,
    button::{Button, ButtonVariants},
};
use wallet_ops::vault::{PublicAccountMetadata, PublicAccountSource};

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
