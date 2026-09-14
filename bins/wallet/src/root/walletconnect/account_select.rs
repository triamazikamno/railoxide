use super::*;

impl SelectItem for WalletConnectAccountSelectItem {
    type Value = Arc<str>;

    fn title(&self) -> SharedString {
        SharedString::from(walletconnect_account_select_summary(self))
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(walletconnect_account_select_row(self, false).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        walletconnect_account_select_row(self, true)
    }

    fn value(&self) -> &Self::Value {
        &self.public_account_uuid
    }

    fn matches(&self, query: &str) -> bool {
        walletconnect_account_matches_search(self, query)
    }
}

pub(super) fn public_account_walletconnect_label(account: &PublicAccountMetadata) -> String {
    let label = account.label.as_deref().unwrap_or("Public account");
    format!("{} · {}", label, short_address(&account.address))
}

pub(in crate::root) fn walletconnect_account_select_items(
    accounts: &[PublicAccountMetadata],
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Vec<WalletConnectAccountSelectItem> {
    accounts
        .iter()
        .map(|account| WalletConnectAccountSelectItem {
            public_account_uuid: Arc::from(account.public_account_uuid.as_str()),
            label: Arc::from(
                account
                    .label
                    .as_deref()
                    .unwrap_or("Public account")
                    .to_owned(),
            ),
            address: account.address,
            usd_total_label: public_account_usd_total_label_for_chain(
                snapshot,
                chain_id,
                &account.public_account_uuid,
                account.status,
                anchor_cache,
            )
            .map(Arc::from),
        })
        .collect()
}

pub(super) fn walletconnect_account_select_index(
    items: &[WalletConnectAccountSelectItem],
    selected_uuid: Option<&Arc<str>>,
) -> Option<IndexPath> {
    let selected_uuid = selected_uuid?;
    items
        .iter()
        .position(|item| item.public_account_uuid.as_ref() == selected_uuid.as_ref())
        .map(|index| IndexPath::default().row(index))
}

pub(super) fn sync_walletconnect_account_select_entity(
    select: &Entity<SelectState<FullWidthSelectItems<WalletConnectAccountSelectItem>>>,
    accounts: &[PublicAccountMetadata],
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    anchor_cache: Option<&TokenAnchorRateCache>,
    selected_uuid: Option<&Arc<str>>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let items = walletconnect_account_select_items(accounts, snapshot, chain_id, anchor_cache);
    let selected_index = walletconnect_account_select_index(&items, selected_uuid);
    select.update(cx, |select, cx| {
        select.set_items(FullWidthSelectItems::new(items), window, cx);
        select.set_selected_index(selected_index, window, cx);
    });
}

pub(in crate::root) fn normalized_walletconnect_account_uuid(
    selected_uuid: Option<&Arc<str>>,
    accounts: &[PublicAccountMetadata],
) -> Option<Arc<str>> {
    selected_uuid
        .filter(|uuid| {
            accounts
                .iter()
                .any(|account| account.public_account_uuid.as_str() == uuid.as_ref())
        })
        .cloned()
        .or_else(|| {
            accounts
                .first()
                .map(|account| Arc::from(account.public_account_uuid.as_str()))
        })
}

pub(super) fn walletconnect_account_select_row(
    item: &WalletConnectAccountSelectItem,
    include_details: bool,
) -> gpui::Div {
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap_3()
        .when(include_details, gpui::Styled::py_1)
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_color(rgb(theme::TEXT))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(SharedString::from(if include_details {
                            item.label.to_string()
                        } else {
                            walletconnect_account_select_summary(item)
                        })),
                )
                .when(include_details, |this| {
                    this.child(
                        div()
                            .truncate()
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(px(11.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(short_address(&item.address)),
                    )
                }),
        )
        .when(include_details, |row| {
            row.when_some(item.usd_total_label.as_ref(), |row, balance| {
                row.child(
                    app_muted_text(SharedString::from(balance.to_string()))
                        .debug_selector(|| format!("walletconnect-balance-{}", item.label))
                        .flex_none()
                        .text_right(),
                )
            })
        })
}

pub(super) fn walletconnect_account_select_summary(
    item: &WalletConnectAccountSelectItem,
) -> String {
    walletconnect_join_account_parts(
        &item.label,
        short_address(&item.address),
        item.usd_total_label.as_deref(),
    )
}

pub(super) fn walletconnect_join_account_parts(
    first: impl AsRef<str>,
    second: impl AsRef<str>,
    third: Option<&str>,
) -> String {
    [Some(first.as_ref()), Some(second.as_ref()), third]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(in crate::root) fn walletconnect_account_matches_search(
    item: &WalletConnectAccountSelectItem,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    let full_address = item.address.to_checksum(None).to_ascii_lowercase();
    let lower_hex_address = format!("{:#x}", item.address);
    let short = short_address(&item.address).to_ascii_lowercase();
    item.label.to_ascii_lowercase().contains(&query)
        || full_address.contains(&query)
        || lower_hex_address.contains(&query)
        || short.contains(&query)
        || item
            .public_account_uuid
            .to_ascii_lowercase()
            .contains(&query)
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[gpui::test]
    fn walletconnect_balances_align_at_menu_edge_and_rows_remain_selectable(
        cx: &mut gpui::TestAppContext,
    ) {
        let items = [("A", "$1.00"), ("Longer account", "$123.45")]
            .into_iter()
            .map(|(label, balance)| WalletConnectAccountSelectItem {
                public_account_uuid: Arc::from(label),
                label: Arc::from(label),
                address: alloy::primitives::Address::ZERO,
                usd_total_label: Some(Arc::from(balance)),
            })
            .collect();
        crate::root::ui_helpers::select_layout_test::assert_balances_align_and_rows_select(
            cx,
            items,
            [500.0, 320.0],
            true,
            [
                "walletconnect-balance-A",
                "walletconnect-balance-Longer account",
            ],
            "Longer account",
        );
    }
}
