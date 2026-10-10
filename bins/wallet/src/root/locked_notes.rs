//! Locked notes: notes kept out of private spending, why, and what the user can do about each
//! lock. Stealth-account operations can be checked against the chain or released, local
//! pending-submission locks can be cleared, and chain pending spends are shown for context.
//!
//! Nothing here reads the chain on its own. "Check again" reads one operation's executor
//! account at the confirmed block, only when the user asks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{Disableable as _, Sizable as _, WindowExt as _, button::ButtonVariants as _};
use railgun_ui::short_address;
use ui::controls::{app_button, app_muted_text, app_strong_text, app_text};
use ui::theme;
use wallet_ops::{
    ExecutorInputLock, ExecutorInputLockKind, ExecutorInputLockReason, LockedNotes,
    WalletNoteLocks, WalletSession, vault::ExecutorOperationId,
};

use super::private_swap::{local_time_label, now_unix};
use super::utxo::{local_pending_spent_card, local_pending_spent_summary};
use super::{
    PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH, WalletRoot, dialog_max_height,
    format_token_amount_for_display, rgb_with_alpha, secondary_dialog_content_width,
};

const RELEASE_WARNING: &str = "Releasing lets the wallet spend these notes again. The signed transaction can still be submitted; whichever transaction spends these notes first succeeds and the other fails. If the old one lands, you pay its approved fee.";
const ORDER_RELEASE_WARNING: &str = "The swap order can still fill until it expires.";

/// Open the locked notes dialog above any open dialog, such as the swap form.
pub(super) fn open_locked_notes_dialog(
    root: WeakEntity<WalletRoot>,
    session: Arc<WalletSession>,
    runtime: tokio::runtime::Handle,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| LockedNotesView::new(root, session, runtime, cx));
    window.open_dialog(cx, move |dialog, window, _| {
        let width =
            (window.viewport_size().width * 0.92).min(PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH);
        dialog
            .w(width)
            .max_h(dialog_max_height(window))
            .on_ok(|_, _, _| false)
            .title(app_strong_text("Locked notes"))
            .child(
                div()
                    .w(secondary_dialog_content_width(width))
                    .child(view.clone()),
            )
    });
}

impl WalletRoot {
    /// The confirmed block of `chain_id`'s last synced head.
    pub(super) fn confirmed_block(&self, chain_id: u64) -> Option<u64> {
        let head = self.chain_states.get(&chain_id)?.sync_tip()?.head_block?;
        let depth = self.effective_chain_configs.get(chain_id)?.finality_depth;
        Some(head.saturating_sub(depth))
    }

    pub(super) fn open_locked_notes(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(session) = self.selected_chain_session() else {
            return;
        };
        open_locked_notes_dialog(
            cx.entity().downgrade(),
            session,
            self.runtime.clone(),
            window,
            cx,
        );
    }

    /// The Activity tab's summary of notes that stealth-account operations reserve.
    pub(super) fn render_executor_lock_summary(&self, root: &Entity<Self>) -> Option<gpui::Div> {
        self.selected_chain_session()?.executor_owner()?;
        let count = self.executor_locked_note_count;
        if count == 0 {
            return None;
        }
        let noun = if count == 1 { "note" } else { "notes" };
        let root = root.clone();
        Some(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::SURFACE))
                .p(px(10.0))
                .child(app_muted_text(format!(
                    "{count} {noun} locked by stealth-account operations"
                )))
                .child(div().flex_1())
                .child(
                    app_button("wallet-review-locked-notes", "Review")
                        .outline()
                        .small()
                        .on_click(move |_, window, cx| {
                            root.update(cx, |root, cx| root.open_locked_notes(window, cx));
                        }),
                ),
        )
    }
}

/// The dialog's content. It reads the wallet session only, so it can be created while the
/// root is being updated; the root is read only while rendering or handling input.
struct LockedNotesView {
    root: WeakEntity<WalletRoot>,
    session: Arc<WalletSession>,
    runtime: tokio::runtime::Handle,
    locks: Result<WalletNoteLocks, String>,
    checking: BTreeSet<ExecutorOperationId>,
    errors: BTreeMap<ExecutorOperationId, String>,
    /// The operation whose release awaits confirmation.
    releasing: Option<ExecutorOperationId>,
    clear_confirming: bool,
    _changes: Task<()>,
}

impl LockedNotesView {
    fn new(
        root: WeakEntity<WalletRoot>,
        session: Arc<WalletSession>,
        runtime: tokio::runtime::Handle,
        cx: &Context<'_, Self>,
    ) -> Self {
        let mut executor_changes = session.executor_owner().map(|owner| owner.subscribe());
        let mut private_changes = session.observation_rx.clone();
        let changes = cx.spawn(async move |this, cx| {
            loop {
                let changed = match executor_changes.as_mut() {
                    Some(executor_changes) => tokio::select! {
                        changed = executor_changes.changed() => changed,
                        changed = private_changes.changed() => changed,
                    },
                    None => private_changes.changed().await,
                };
                if changed.is_err()
                    || this
                        .update(cx, |this, cx| {
                            this.refresh();
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        });
        let mut view = Self {
            root,
            session,
            runtime,
            locks: Ok(WalletNoteLocks::default()),
            checking: BTreeSet::new(),
            errors: BTreeMap::new(),
            releasing: None,
            clear_confirming: false,
            _changes: changes,
        };
        view.refresh();
        view
    }

    fn refresh(&mut self) {
        self.locks = self
            .session
            .note_locks()
            .map_err(|error| format!("{error:#}"));
        if let Ok(locks) = &self.locks {
            if self
                .releasing
                .is_some_and(|operation| !has_lock(locks, operation))
            {
                self.releasing = None;
            }
            if locks.local_pending().is_empty() {
                self.clear_confirming = false;
            }
        }
    }

    fn confirmed_block(&self, cx: &App) -> Option<u64> {
        self.root
            .upgrade()?
            .read(cx)
            .confirmed_block(self.session.chain_id)
    }

    fn amounts_label(&self, notes: &LockedNotes, cx: &App) -> String {
        let root = self.root.upgrade();
        let registry = root
            .as_ref()
            .map(|root| &root.read(cx).effective_token_registry);
        notes
            .amounts()
            .map(|(token, amount)| {
                format_token_amount_for_display(self.session.chain_id, token, amount, registry)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Read one operation's account at the confirmed block. This is the only chain read here.
    fn check(&mut self, operation: ExecutorOperationId, cx: &mut Context<'_, Self>) {
        let Some(confirmed) = self.confirmed_block(cx) else {
            return;
        };
        if !self.checking.insert(operation) {
            return;
        }
        self.errors.remove(&operation);
        let session = Arc::clone(&self.session);
        let check = self
            .runtime
            .spawn(async move { session.check_input_lock(operation, confirmed).await });
        cx.spawn(async move |this, cx| {
            let result = check.await;
            let _ = this.update(cx, |this, cx| {
                this.checking.remove(&operation);
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        this.errors.insert(operation, format!("{error:#}"));
                    }
                    Err(_) => {
                        this.errors
                            .insert(operation, "The check stopped unexpectedly.".into());
                    }
                }
                this.refresh();
                cx.notify();
                this.sync_root(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Refresh the root's locked-note summary after a check or release changed locks.
    fn sync_root(&self, cx: &mut App) {
        let _ = self.root.update(cx, |root, cx| {
            root.sync_utxo_table(cx);
            cx.notify();
        });
    }

    fn release(&mut self, operation: ExecutorOperationId, cx: &mut Context<'_, Self>) {
        self.releasing = None;
        let result = match self.session.executor_owner() {
            Some(owner) => owner
                .release_input_lock(operation)
                .map_err(|error| format!("{error:#}")),
            None => Err("Stealth accounts are unavailable for this wallet session.".into()),
        };
        match result {
            Ok(()) => {
                self.errors.remove(&operation);
            }
            Err(error) => {
                self.errors.insert(operation, error);
            }
        }
        self.refresh();
        cx.notify();
        self.sync_root(cx);
    }

    fn clear_local_pending(&mut self, cx: &mut Context<'_, Self>) {
        self.clear_confirming = false;
        let session = Arc::clone(&self.session);
        let clear = self
            .runtime
            .spawn(async move { session.clear_local_pending_spent().await });
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            if clear.await.unwrap_or(false) {
                let _ = root.update(cx, |root, cx| {
                    root.sync_utxo_table(cx);
                    cx.notify();
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.refresh();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_executor_lock(&self, lock: &ExecutorInputLock, cx: &Context<'_, Self>) -> gpui::Div {
        let operation = lock.operation();
        let id = operation.opaque_id();
        let checking = self.checking.contains(&operation);
        let confirming = self.releasing == Some(operation);
        // The check reads the account's own state, so it needs its address.
        let can_check = lock.address().is_some() && self.confirmed_block(cx).is_some();
        let account = lock.address().map_or_else(
            || "address not derived".to_owned(),
            |address| short_address(&address),
        );
        let order_open = matches!(lock.reason(), ExecutorInputLockReason::OrderOpen { .. });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(if confirming {
                theme::DANGER
            } else {
                theme::BORDER
            }))
            .bg(if confirming {
                rgb_with_alpha(theme::DANGER, 0.08)
            } else {
                rgb(theme::SURFACE)
            })
            .p(px(10.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                app_strong_text(self.amounts_label(lock.notes(), cx))
                                    .whitespace_normal(),
                            )
                            .child(app_muted_text(format!(
                                "{} · {account}",
                                kind_label(lock.kind())
                            )))
                            .child(app_muted_text(reason_label(lock.reason())).whitespace_normal()),
                    )
                    .when(!confirming, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .flex()
                                .gap_2()
                                .child(
                                    app_button(
                                        SharedString::from(format!("locked-notes-check-{id}")),
                                        "Check again",
                                    )
                                    .outline()
                                    .small()
                                    .loading(checking)
                                    .disabled(checking || !can_check)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.check(operation, cx);
                                        },
                                    )),
                                )
                                .child(
                                    app_button(
                                        SharedString::from(format!("locked-notes-release-{id}")),
                                        "Release…",
                                    )
                                    .outline()
                                    .small()
                                    .danger()
                                    .disabled(checking)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.releasing = Some(operation);
                                            cx.notify();
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .children(self.errors.get(&operation).map(|error| {
                app_text(error.clone())
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal()
            }))
            .when(confirming, |card| {
                card.child(
                    app_text(RELEASE_WARNING)
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
                .when(order_open, |card| {
                    card.child(
                        app_text(ORDER_RELEASE_WARNING)
                            .text_color(rgb(theme::DANGER))
                            .whitespace_normal(),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_button(
                                SharedString::from(format!("locked-notes-cancel-release-{id}")),
                                "Cancel",
                            )
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.releasing = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            app_button(
                                SharedString::from(format!("locked-notes-confirm-release-{id}")),
                                "Release",
                            )
                            .small()
                            .danger()
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.release(operation, cx);
                                },
                            )),
                        ),
                )
            })
    }

    fn render_local_pending(&self, notes: &LockedNotes, cx: &Context<'_, Self>) -> gpui::Div {
        let begin = cx.entity();
        let cancel = begin.clone();
        let confirm = begin.clone();
        local_pending_spent_card(
            "locked-notes",
            format!(
                "{} · {}",
                local_pending_spent_summary(notes.count()),
                self.amounts_label(notes, cx)
            ),
            self.clear_confirming,
            move |_, cx| {
                begin.update(cx, |this, cx| {
                    this.clear_confirming = true;
                    cx.notify();
                });
            },
            move |_, cx| {
                cancel.update(cx, |this, cx| {
                    this.clear_confirming = false;
                    cx.notify();
                });
            },
            move |_, cx| {
                confirm.update(cx, Self::clear_local_pending);
            },
        )
    }

    fn render_chain_pending(&self, notes: &LockedNotes, cx: &App) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p(px(10.0))
            .child(app_strong_text(self.amounts_label(notes, cx)).whitespace_normal())
            .child(app_muted_text("A pending transaction spends these notes"))
    }
}

impl Render for LockedNotesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let locks = match &self.locks {
            Ok(locks) => locks,
            Err(error) => {
                return div().w_full().child(
                    app_text(error.clone())
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                );
            }
        };
        let empty = locks.executor().is_empty()
            && locks.local_pending().is_empty()
            && locks.chain_pending().is_empty();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .when(empty, |content| {
                content.child(app_muted_text("No locked notes."))
            })
            .children(
                locks
                    .executor()
                    .iter()
                    .map(|lock| self.render_executor_lock(lock, cx)),
            )
            .when(!locks.local_pending().is_empty(), |content| {
                content.child(self.render_local_pending(locks.local_pending(), cx))
            })
            .when(!locks.chain_pending().is_empty(), |content| {
                content.child(self.render_chain_pending(locks.chain_pending(), cx))
            })
    }
}

fn has_lock(locks: &WalletNoteLocks, operation: ExecutorOperationId) -> bool {
    locks
        .executor()
        .iter()
        .any(|lock| lock.operation() == operation)
}

const fn kind_label(kind: ExecutorInputLockKind) -> &'static str {
    match kind {
        ExecutorInputLockKind::SwapSetup => "Swap setup",
        ExecutorInputLockKind::SwapOrder => "Swap order",
        ExecutorInputLockKind::Recovery => "Recovery",
        ExecutorInputLockKind::Operation => "Stealth-account operation",
    }
}

fn reason_label(reason: ExecutorInputLockReason) -> String {
    match reason {
        ExecutorInputLockReason::NeedsChainCheck => {
            "Needs a chain check since the wallet restarted".into()
        }
        ExecutorInputLockReason::SignedNotConfirmed => {
            "A signed transaction can still execute".into()
        }
        ExecutorInputLockReason::OrderOpen { valid_to } => {
            let valid_to = u64::from(valid_to);
            if valid_to > now_unix() {
                format!("Order open until {}", local_time_label(valid_to))
            } else {
                format!(
                    "Order expired at {}, waiting for the chain to confirm it",
                    local_time_label(valid_to)
                )
            }
        }
        ExecutorInputLockReason::ResolvedAwaitingSync => {
            "Nonce used, waiting for private sync".into()
        }
    }
}
