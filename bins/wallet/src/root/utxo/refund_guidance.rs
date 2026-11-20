#[allow(clippy::wildcard_imports)]
use super::*;

/// The title, fix and action the refund dialog shows for the `blocker` of an unavailable
/// refund. `stealth_accounts` says whether the selected chain has Stealth accounts to open, and
/// `native` is the symbol of its native currency.
pub(super) fn blocked_shield_refund_guidance(
    blocker: Option<&BlockedShieldRescueBlocker>,
    stealth_accounts: bool,
    native: Option<&str>,
) -> BlockedShieldRefundGuidance {
    match blocker {
        None => BlockedShieldRefundGuidance {
            title: "Refund unavailable",
            fix: None,
            action: None,
        },
        Some(BlockedShieldRescueBlocker::OriginUnknown) if stealth_accounts => {
            BlockedShieldRefundGuidance {
                title: "Origin account missing",
                fix: Some(
                    "A blocked Shield is refunded to the account that made it, and that account \
                     pays the gas. If it was one of your stealth accounts, restore it in Stealth \
                     accounts. Otherwise add it to Public."
                        .into(),
                ),
                action: Some((
                    "Open Stealth accounts",
                    BlockedShieldRefundAction::OpenStealthAccounts,
                )),
            }
        }
        Some(BlockedShieldRescueBlocker::OriginUnknown) => BlockedShieldRefundGuidance {
            title: "Origin account missing",
            fix: Some(
                "A blocked Shield is refunded to the account that made it, and that account pays \
                 the gas. Add it to Public, then refund."
                    .into(),
            ),
            action: None,
        },
        Some(BlockedShieldRescueBlocker::OriginStealth { operation, .. }) => {
            BlockedShieldRefundGuidance {
                title: "Stealth account not in Public",
                fix: Some(
                    format!(
                        "Add it to Public, send it {} for gas, then refund.",
                        native.unwrap_or("native currency")
                    )
                    .into(),
                ),
                action: Some((
                    "Show stealth account",
                    BlockedShieldRefundAction::ShowStealthAccount(*operation),
                )),
            }
        }
        Some(BlockedShieldRescueBlocker::OriginInactive { .. }) => BlockedShieldRefundGuidance {
            title: "Origin account inactive",
            fix: Some("Activate it, then refund.".into()),
            action: Some(("Open Public", BlockedShieldRefundAction::OpenPublic)),
        },
    }
}

/// The Close button of the refund dialog.
pub(super) fn blocked_shield_refund_close_button() -> Button {
    app_button("wallet-blocked-shield-refund-unavailable-close", "Close")
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// The "Origin account" row of the refund dialog: the address beside its copy button.
pub(super) fn blocked_shield_refund_origin_row(address: String) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Origin account"))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_text(address.clone())
                        .font_family(APP_MONO_FONT_FAMILY)
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(
                    clipboard_with_toast("wallet-blocked-shield-refund-origin-copy", address)
                        .tooltip("Copy origin account address"),
                ),
        )
}

/// The footer of the refund dialog for an available refund: Close beside the button that
/// goes on to the refund's authorization.
pub(super) fn blocked_shield_refund_available_footer(
    root: &Entity<WalletRoot>,
    utxo_id: BlockedShieldRescueUtxoId,
) -> gpui::AnyElement {
    let root = root.clone();
    DialogFooter::new()
        .child(blocked_shield_refund_close_button().ghost())
        .child(
            app_button("wallet-blocked-shield-refund-continue", "Refund…")
                .primary()
                .debug_selector(|| "wallet-blocked-shield-refund-continue".into())
                .on_click(move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        window.close_dialog(cx);
                        // The row is read again, so the refund goes on only while the row
                        // still offers it.
                        if let Some(row) = root.active_blocked_shield_rescue_display_row(utxo_id) {
                            root.begin_blocked_shield_refund(&row, window, cx);
                        }
                    });
                }),
        )
        .into_any_element()
}

/// The footer of the refund dialog for an unavailable refund: Close alone, or Close beside
/// the `action` that leads to the fix. The action is disabled while it can't be followed.
pub(super) fn blocked_shield_refund_unavailable_footer(
    root: &Entity<WalletRoot>,
    action: Option<(&'static str, BlockedShieldRefundAction)>,
    cx: &App,
) -> gpui::AnyElement {
    let close = blocked_shield_refund_close_button();
    let Some((label, action)) = action else {
        return close.into_any_element();
    };
    let available = root
        .read(cx)
        .can_follow_blocked_shield_refund_action(action, cx);
    let root = root.clone();
    DialogFooter::new()
        .child(close.ghost())
        .child(
            app_button("wallet-blocked-shield-refund-unavailable-action", label)
                .primary()
                .disabled(!available)
                .debug_selector(|| "wallet-blocked-shield-refund-unavailable-action".into())
                .on_click(move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        // The dialog stays open while the action can't be followed, so its
                        // guidance isn't lost.
                        if !root.can_follow_blocked_shield_refund_action(action, cx) {
                            return;
                        }
                        // Close every dialog, so nothing opens underneath one. This dialog
                        // opened inside a Refund click, over nothing or over "Private asset
                        // status". A dialog opened since covers this button until it is
                        // dismissed, so no other dialog is open now.
                        window.close_all_dialogs(cx);
                        root.follow_blocked_shield_refund_action(action, window, cx);
                    });
                }),
        )
        .into_any_element()
}

pub(super) const fn blocked_shield_rescue_error_info(error: String) -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(error),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    }
}

pub(super) fn blocked_shield_rescue_in_flight_info(
    base: Option<&BlockedShieldRescueInfo>,
) -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(BLOCKED_SHIELD_REFUND_IN_FLIGHT_REASON.to_string()),
        origin_address: base.and_then(|info| info.origin_address.clone()),
        public_account_uuid: base.and_then(|info| info.public_account_uuid.clone()),
        public_account_label: base.and_then(|info| info.public_account_label.clone()),
    }
}

pub(super) fn blocked_shield_rescue_submitted_info() -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(BLOCKED_SHIELD_REFUND_SUBMITTED_REASON.to_string()),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    }
}
