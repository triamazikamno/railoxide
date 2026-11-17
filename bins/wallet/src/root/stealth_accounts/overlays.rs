use super::token_picker::TokenPicker;
use super::{
    Context, Disableable, ExecutorAsset, ExecutorOperationId, Focusable, IntoElement,
    ParentElement, Sizable, StealthAccountsView, Styled, Window, app_button, app_input,
    app_muted_text, app_strong_text, div, labeled_field,
};
use gpui::{AppContext as _, Entity, InteractiveElement as _, prelude::FluentBuilder as _, rems};
use gpui_component::WindowExt as _;
use gpui_component::{
    ActiveTheme as _,
    button::ButtonVariants as _,
    dialog::{Cancel, Confirm, Dialog, DialogFooter},
    input::InputState,
};
use ui::dialog::WalletDialogExt as _;

impl StealthAccountsView {
    pub(super) fn open_add_token(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        let Some(record) = self
            .records
            .iter()
            .find(|record| record.operation() == operation)
        else {
            return;
        };
        let index = record.index();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let registry = root.read(cx).effective_token_registry.clone();
        let picker = cx.new(|cx| TokenPicker::new(&registry, self.session.chain_id, window, cx));
        cx.observe(&picker, |view, _, cx| {
            view.error = None;
            cx.notify();
        })
        .detach();
        let focus_picker = picker.clone();
        self.asset_kind = super::AssetKind::Erc20;
        self.token_address
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.token_id
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        let view = cx.entity().downgrade();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let content = view
                .update(cx, |view, cx| {
                    if !view.session_is_current(cx) {
                        return div().child(app_muted_text("Wallet session ended."));
                    }
                    view.render_asset_fields(false, Some(&picker), cx).children(
                        view.error.as_ref().map(|error| {
                            app_muted_text(error.clone())
                                .text_color(cx.theme().danger)
                                .whitespace_normal()
                        }),
                    )
                })
                .unwrap_or_else(|_| div().child(app_muted_text("Wallet session ended.")));
            let submit_view = view.clone();
            let submit_picker = picker.clone();
            let click_view = view.clone();
            let click_picker = picker.clone();
            let return_view = view.clone();
            dialog
                .title(app_strong_text(format!("Add token to account #{index}")))
                .w((window.viewport_size().width * 0.92)
                    .min(rems(30.).to_pixels(window.rem_size())))
                .max_h(window.viewport_size().height * 0.9)
                .child(content)
                .footer(
                    DialogFooter::new()
                        .child(
                            app_button("cancel", "Cancel")
                                .small()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(Cancel), cx);
                                }),
                        )
                        .child(
                            app_button("ok", "Add token")
                                .small()
                                .primary()
                                .debug_selector(|| "stealth-add-token-submit".to_owned())
                                .on_click(move |_, window, cx| {
                                    // Confirm would be handled by the focused token selector.
                                    let added = click_view
                                        .update(cx, |view, cx| {
                                            view.add_token(operation, &click_picker, cx)
                                        })
                                        .unwrap_or(false);
                                    if added {
                                        window.close_dialog(cx);
                                        let return_view = click_view.clone();
                                        // Restore keyboard focus after the modal focus trap is gone.
                                        window.on_next_frame(move |window, cx| {
                                            if !window.has_active_dialog(cx) {
                                                let _ = return_view.update(cx, |view, cx| {
                                                    view.finish_add_token_dialog(window, cx);
                                                });
                                            }
                                        });
                                    }
                                }),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    submit_view
                        .update(cx, |view, cx| view.add_token(operation, &submit_picker, cx))
                        .unwrap_or(false)
                })
                .on_close(move |_, window, cx| {
                    let _ = return_view.update(cx, |view, cx| {
                        view.finish_add_token_dialog(window, cx);
                    });
                })
        });
        cx.defer_in(window, move |_, window, cx| {
            focus_picker.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    fn finish_add_token_dialog(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.error = None;
        self.add_token_focus.focus(window, cx);
        window.focus_next(cx);
        cx.notify();
    }

    fn add_token(
        &mut self,
        operation: ExecutorOperationId,
        picker: &Entity<TokenPicker>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.job.is_some()
            || !self.session_is_current(cx)
            || !self
                .records
                .iter()
                .any(|record| record.operation() == operation)
        {
            return false;
        }
        let asset = if self.asset_kind == super::AssetKind::Erc20 {
            picker.read(cx).token(cx).map(ExecutorAsset::Erc20)
        } else {
            self.selected_asset(cx)
        };
        let asset = match asset {
            Ok(asset) => asset,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return false;
            }
        };
        self.observations
            .entry(operation)
            .or_default()
            .assets
            .entry(asset)
            .or_default();
        self.refresh_visible(cx);
        cx.notify();
        true
    }

    pub(super) fn recovery_disabled_reason(
        &self,
        record: &super::ExecutorRecord,
        cx: &gpui::App,
    ) -> Option<&'static str> {
        if !self.session_is_current(cx) {
            return Some("Unlock this wallet again to recover funds.");
        }
        if self.job.is_some() {
            return Some("Wait for the current operation to finish or stop it first.");
        }
        if record.address().is_none() {
            return Some("Restore this account's address before recovering funds.");
        }
        if self.holding(record.operation()) || super::view::swap_holds_funds(&self.owner, record) {
            return None;
        }
        let checked = self
            .observations
            .get(&record.operation())
            .is_some_and(|observations| {
                observations
                    .assets
                    .values()
                    .any(|balance| balance.value.is_some())
            });
        Some(if checked {
            "No positive balances to recover. Check balances or add another asset."
        } else {
            "Check balances to find funds to recover."
        })
    }

    /// Open recovery for `operation`. `return_focus` is the caller's dialog under it, which
    /// gets focus back when recovery closes; without it, the account's actions do.
    pub(super) fn open_recovery(
        &mut self,
        operation: ExecutorOperationId,
        asset: Option<ExecutorAsset>,
        return_focus: Option<gpui::FocusHandle>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self
            .records
            .iter()
            .find(|record| record.operation() == operation)
        else {
            return;
        };
        if self.recovery_disabled_reason(record, cx).is_some() {
            return;
        }
        let index = record.index();
        let asset = asset
            .or_else(|| {
                self.observations.get(&operation).and_then(|observations| {
                    observations
                        .assets
                        .iter()
                        .filter_map(|(asset, balance)| {
                            balance
                                .value
                                .is_some_and(|value| !value.amount.is_zero())
                                .then_some(*asset)
                        })
                        .min_by_key(|asset| *asset == ExecutorAsset::Native)
                })
            })
            .or_else(|| {
                record
                    .assets()
                    .iter()
                    .find(|asset| **asset != ExecutorAsset::Native)
                    .copied()
            })
            .unwrap_or(ExecutorAsset::Native);
        self.expanded = Some(operation);
        self.refresh_visible(cx);
        self.select_recovery_account(operation, cx);
        self.recovery.open = true;
        self.recovery.client = self
            .root
            .upgrade()
            .map(|root| wallet_ops::OperationHttpClientSlot::new(root.read(cx).http.clone()));
        self.recovery.gas_quote = None;
        self.recovery.native_funding = self.recovery_has_native_balance();
        self.sync_recovery_assets(window, cx);
        self.set_recovery_asset(asset, window, cx);
        self.start_recovery_updates(window, cx);
        let view = cx.entity().downgrade();
        let return_view = view.clone();
        // The dialog returns focus to what was focused when it opened.
        if let Some(focus) = &return_focus {
            focus.focus(window, cx);
        }
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let content = view
                .update(cx, |view, cx| {
                    if !view.session_is_current(cx) {
                        return div()
                            .child(app_muted_text("Wallet session ended."))
                            .into_any_element();
                    }
                    div()
                        .debug_selector(|| "stealth-recovery-form".into())
                        .min_w_0()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(labeled_field(
                            "Asset",
                            div()
                                .debug_selector(|| "stealth-recovery-select".into())
                                .child(ui::private_action::asset_select(
                                    &view.recovery.asset_select,
                                    view.job.is_some(),
                                )),
                        ))
                        .child(view.render_recovery_form(cx))
                        .child(view.render_work_status(cx))
                        .into_any_element()
                })
                .unwrap_or_else(|_| {
                    div()
                        .child(app_muted_text("Wallet session ended."))
                        .into_any_element()
                });
            let return_view = return_view.clone();
            let return_focus = return_focus.clone();
            dialog
                .title(app_strong_text(format!("Recover from account #{index}")))
                .w((window.viewport_size().width * 0.92)
                    .min(rems(36.).to_pixels(window.rem_size())))
                .max_h(window.viewport_size().height * 0.88)
                .on_ok(|_, _, _| false)
                .overlay_closable(false)
                .child(content)
                .on_close(move |_, window, cx| {
                    let _ = return_view.update(cx, |view, cx| {
                        view.close_recovery();
                        return_focus
                            .as_ref()
                            .unwrap_or(&view.recover_focus)
                            .focus(window, cx);
                        cx.notify();
                    });
                })
        });
        cx.defer_in(window, |view, window, cx| {
            view.focus_recovery_amount(window, cx);
        });
        cx.notify();
    }

    pub(super) fn open_restore(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        self.pending_authorization = None;
        let view = cx.entity().downgrade();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog = match view.upgrade() {
                Some(view) if view.read(cx).session_is_current(cx) => {
                    let view = view.read(cx);
                    restore_dialog(
                        dialog,
                        &view.range_start,
                        &view.range_count,
                        view.job.is_some(),
                        window,
                        cx,
                    )
                }
                _ => dialog
                    .title(app_strong_text("Restore accounts"))
                    .child(app_muted_text("Wallet session ended.")),
            };
            let submit_view = view.clone();
            let return_view = view.clone();
            dialog
                .on_ok(move |_, window, cx| {
                    submit_view
                        .update(cx, |view, cx| {
                            if view.job.is_some() || !view.session_is_current(cx) {
                                return false;
                            }
                            let Ok(range) = restore_range(
                                &view.range_start.read(cx).value(),
                                &view.range_count.read(cx).value(),
                            ) else {
                                return false;
                            };
                            // Finish closing the form before opening its authorization dialog.
                            cx.defer_in(window, move |view, window, cx| {
                                view.request_discovery(range, window, cx);
                            });
                            true
                        })
                        .unwrap_or(false)
                })
                .on_close(move |_, window, cx| {
                    let _ = return_view.update(cx, |view, cx| {
                        view.pending_authorization = None;
                        view.search.read(cx).focus_handle(cx).focus(window, cx);
                        cx.notify();
                    });
                })
        });
        cx.defer_in(window, |view, window, cx| {
            view.range_start.read(cx).focus_handle(cx).focus(window, cx);
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreRangeError {
    FirstIndex,
    Count,
    EndIndex,
}

impl RestoreRangeError {
    const fn message(self) -> &'static str {
        match self {
            Self::FirstIndex => "Choose a first index from 0 to 2147483647.",
            Self::Count => "Choose 1 to 64 accounts.",
            Self::EndIndex => "Choose a range ending below index 2147483648.",
        }
    }
}

fn restore_range(start: &str, count: &str) -> Result<std::ops::Range<u32>, RestoreRangeError> {
    let start = start
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|start| *start < 1 << 31)
        .ok_or(RestoreRangeError::FirstIndex)?;
    let count = count
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|count| (1..=64).contains(count))
        .ok_or(RestoreRangeError::Count)?;
    let end = start
        .checked_add(count)
        .filter(|end| *end <= 1 << 31)
        .ok_or(RestoreRangeError::EndIndex)?;
    Ok(start..end)
}

fn restore_dialog(
    dialog: Dialog,
    range_start: &Entity<InputState>,
    range_count: &Entity<InputState>,
    busy: bool,
    window: &Window,
    cx: &gpui::App,
) -> Dialog {
    let range = restore_range(&range_start.read(cx).value(), &range_count.read(cx).value());
    let error = range.as_ref().err().copied();
    let description = match &range {
        Ok(range) => format!(
            "Derives accounts #{}–#{} from this wallet's seed and asks the endpoint whether each has been used, one address per request, a few seconds apart. Balances are not checked.",
            range.start,
            range.end - 1
        ),
        Err(error) => error.message().to_owned(),
    };
    let content = div()
        .min_w_0()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .gap_3()
                .child(
                    div().flex_1().min_w_0().child(labeled_field(
                        "First index",
                        app_input(range_start)
                            .aria_label("First index")
                            .disabled(busy)
                            .when(
                                matches!(
                                    error,
                                    Some(
                                        RestoreRangeError::FirstIndex | RestoreRangeError::EndIndex
                                    )
                                ),
                                |input| input.border_color(cx.theme().warning),
                            ),
                    )),
                )
                .child(
                    div().flex_1().min_w_0().child(labeled_field(
                        "Count",
                        app_input(range_count)
                            .aria_label("Count")
                            .disabled(busy)
                            .when(
                                matches!(
                                    error,
                                    Some(RestoreRangeError::Count | RestoreRangeError::EndIndex)
                                ),
                                |input| input.border_color(cx.theme().warning),
                            ),
                    )),
                ),
        )
        .child(
            app_muted_text(description)
                .whitespace_normal()
                .when(error.is_some(), |text| text.text_color(cx.theme().warning)),
        );
    dialog
        .title(app_strong_text("Restore accounts"))
        .w((window.viewport_size().width * 0.92).min(rems(30.).to_pixels(window.rem_size())))
        .max_h(window.viewport_size().height * 0.9)
        .child(content)
        .footer(
            DialogFooter::new()
                .child(
                    app_button("stealth-restore-cancel", "Cancel")
                        .ghost()
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                )
                .child(
                    app_button("stealth-restore-review", "Review…")
                        .primary()
                        .disabled(busy || range.is_err())
                        .debug_selector(|| "stealth-restore-review".into())
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
                        }),
                ),
        )
}

#[cfg(test)]
mod tests;
