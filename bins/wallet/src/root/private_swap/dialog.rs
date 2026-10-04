//! The swap dialog. One dialog shows the swap form, My orders, or one swap's order detail, and a
//! back button moves between them. Escape and × close it. The asset and broadcaster pickers,
//! spend authorization review, stealth-account recovery and confirmations open on top of it.
//!
//! Background work may change the dialog only while it has focus and shows the swap the work
//! belongs to, so a quote or order result never replaces or interrupts another dialog.

use alloy::primitives::Address;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, StyledImage as _,
    WeakEntity, WeakFocusHandle, Window, div, img, prelude::FluentBuilder as _, px, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, WindowExt as _,
    button::{ButtonGroup, ButtonVariants as _},
    list::{List, ListDelegate, ListItem, ListState},
    tag::Tag,
};
use ui::controls::{
    app_button, app_button_base, app_button_label, app_muted_text, app_segment_button,
    app_strong_text, app_text,
};
use ui::theme;
use wallet_ops::{
    SwapOrderState, is_swap_record,
    vault::{ExecutorOperationId, ExecutorRecord, SwapDelivery, SwapOrderRecord},
};

use super::model::{
    RecordSwap, SwapIdentity, SwapLabels, SwapOrderGroup, SwapStage, bridge_sent_amount,
    provider_name, record_swaps, swap_delivery, swap_order_group, swap_order_stage,
    swap_order_status, swap_private_minimum,
};
use super::{PrivateSwapsView, local_date_time_label, swap_tokens};
use crate::assets::{
    COW_DAO_LIGHT_PATH, COW_PROTOCOL_LOGO_LIGHT_PATH, LIST_ICON_PATH, WalletIconSource,
};
use crate::root::{dialog_max_height, secondary_dialog_content_width};

/// One column for the form's panels, narrower than Send and Unshield.
const SWAP_DIALOG_WIDTH: Pixels = px(520.0);
/// My orders row height in rems: two lines of body text and the row's padding.
const ORDER_ROW_HEIGHT: f32 = 3.5;
/// Rows My orders shows before it scrolls.
const ORDERS_VISIBLE_ROWS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwapDialogView {
    Form,
    Orders,
    /// The account's latest swap, which owns the live stage and every action.
    Detail(ExecutorOperationId),
    /// An earlier swap on a reused stealth account, by its swap use. Read only. The index is
    /// its first order's, which tells apart the swaps of one use from before swap uses.
    PastDetail(SwapIdentity, usize),
    /// A stopped preparation with no order UID, retained across account reuse.
    CancelledPreparation(SwapIdentity),
}

pub(super) struct SwapDialog {
    pub(super) view: SwapDialogView,
    /// The dialog's own focus. Anything else focused means another surface is in front.
    focus: Option<WeakFocusHandle>,
    /// Whether a traded swap's detail has its fees and its order details expanded. Both
    /// collapse when the view changes.
    pub(super) outcome_fees_open: bool,
    pub(super) outcome_details_open: bool,
}

/// One swap of a record, as My orders lists it.
struct OrderEntry<'a> {
    record: &'a ExecutorRecord,
    /// The detail this swap opens.
    view: SwapDialogView,
    /// The swap's last order, if it has any.
    order: Option<&'a SwapOrderRecord>,
    stage: SwapStage,
    group: SwapOrderGroup,
    started: Option<(&'static str, u64)>,
}

/// One swap in My orders, derived from its record for display only.
struct SwapOrderRow {
    /// The detail this swap opens; also its identity.
    view: SwapDialogView,
    title: String,
    amount: OrderRowAmount,
    meta: String,
    status: String,
    attention: bool,
    icons: [Option<WalletIconSource>; 2],
}

enum OrderRowAmount {
    None,
    /// The minimum while the swap can fill, the received amount, or the stranded amount.
    Value {
        value: String,
        note: Option<&'static str>,
    },
    Muted(&'static str),
}

pub(super) struct SwapOrdersDelegate {
    view: WeakEntity<PrivateSwapsView>,
    rows: Vec<SwapOrderRow>,
    selected: Option<IndexPath>,
}

impl ListDelegate for SwapOrdersDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.rows.len()
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.rows.get(ix.row)?;
        Some(
            ListItem::new(SharedString::from(format!(
                "swap-order-{}",
                row_key(row.view)
            )))
            .h(rems(ORDER_ROW_HEIGHT))
            .px_2()
            .py_0()
            .rounded_md()
            .child(render_order_row(row)),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    /// Enter or a click opens the swap's detail.
    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(target) = self
            .selected
            .and_then(|ix| self.rows.get(ix.row))
            .map(|row| row.view)
        else {
            return;
        };
        let _ = self.view.update(cx, |view, cx| {
            view.navigate(target, window, cx);
        });
    }
}

impl PrivateSwapsView {
    pub(super) fn swap_dialog_focus(&self) -> Option<FocusHandle> {
        self.dialog.as_ref()?.focus.as_ref()?.upgrade()
    }

    /// The swap dialog is open and nothing is in front of it.
    pub(super) fn swap_dialog_active(&self, window: &Window, cx: &App) -> bool {
        self.swap_dialog_focus()
            .is_some_and(|focus| focus.contains_focused(window, cx))
    }

    /// A quote may advance its own swap's detail, but must not interrupt another dialog. Only
    /// the latest swap's detail shows the operation; an earlier swap's detail is read only.
    pub(super) fn detail_is_active(
        &self,
        operation: ExecutorOperationId,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.view == SwapDialogView::Detail(operation))
            && self.swap_dialog_active(window, cx)
    }

    /// The active swap dialog shows `operation`, in its detail or in its form.
    pub(super) fn swap_dialog_shows(
        &self,
        operation: ExecutorOperationId,
        window: &Window,
        cx: &App,
    ) -> bool {
        let shown = match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Detail(shown)) => shown == operation,
            Some(SwapDialogView::Form) => self
                .form
                .as_ref()
                .is_some_and(|form| form.operation() == Some(operation)),
            Some(
                SwapDialogView::Orders
                | SwapDialogView::PastDetail(..)
                | SwapDialogView::CancelledPreparation(_),
            )
            | None => false,
        };
        shown && self.swap_dialog_active(window, cx)
    }

    /// Show `view` in the swap dialog: switch the active dialog, or open a new one in place of
    /// the dialogs that are open.
    pub(super) fn show_view(
        &mut self,
        view: SwapDialogView,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.swap_dialog_active(window, cx) {
            self.navigate(view, window, cx);
        } else {
            self.open_swap_dialog(view, window, cx);
        }
    }

    /// Open one swap's detail. Closing it leaves the swap on the Private tab.
    pub(super) fn show_detail(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.show_view(SwapDialogView::Detail(operation), window, cx);
    }

    /// Switch the open swap dialog's view. A detail replaces the form; My orders keeps it, so
    /// Back returns to it.
    pub(super) fn navigate(
        &mut self,
        view: SwapDialogView,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(dialog) = self.dialog.as_mut() else {
            self.open_swap_dialog(view, window, cx);
            return;
        };
        if dialog.view != view {
            dialog.view = view;
            dialog.outcome_fees_open = false;
            dialog.outcome_details_open = false;
            // A cancelled preparation is reported until the dialog shows something else.
            self.cancelled = None;
        }
        self.close_setup_settings(cx);
        if let SwapDialogView::Detail(_)
        | SwapDialogView::PastDetail(..)
        | SwapDialogView::CancelledPreparation(_) = view
        {
            self.form = None;
            self.error = None;
        }
        self.load_detail_destination(view, cx);
        self.focus_view(view, window, cx);
        cx.notify();
    }

    /// A private Bridge swap's detail tells about its destination stealth account, which only
    /// the destination network's session knows. Opening the detail starts loading that network
    /// and reads the account again.
    fn load_detail_destination(&mut self, view: SwapDialogView, cx: &mut Context<'_, Self>) {
        let delivery = match view {
            SwapDialogView::Detail(operation) => self
                .record(operation)
                .and_then(|record| self.shown_private_delivery(record)),
            SwapDialogView::CancelledPreparation(swap) => self
                .record(swap.operation)
                .and_then(|record| super::model::swap_use_destination(record, swap.swap_use))
                .map(|(delivery, _)| delivery),
            _ => None,
        };
        let Some(delivery) = delivery else {
            return;
        };
        self.ensure_destination_load(delivery.destination_chain, cx);
        self.reload_destinations(cx);
    }

    fn open_swap_dialog(
        &mut self,
        view: SwapDialogView,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.cancelled = None;
        match view {
            SwapDialogView::Form => {}
            SwapDialogView::Orders => self.form = None,
            SwapDialogView::Detail(_)
            | SwapDialogView::PastDetail(..)
            | SwapDialogView::CancelledPreparation(_) => {
                self.form = None;
                self.error = None;
            }
        }
        let entity = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            let width = (window.viewport_size().width * 0.92).min(SWAP_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(width);
            let close_view = entity.clone();
            let rendered = entity.update(cx, |view, cx| {
                let (body, footer) = view.render_dialog_body(content_width, cx);
                (view.render_dialog_title(content_width, cx), body, footer)
            });
            let dialog = dialog
                .w(width)
                .max_h(dialog_max_height(window))
                // The close button sits 8px below the dialog's top edge; this padding centers
                // the title row, a small control tall, on it.
                .pt_2()
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| {
                    let _ = close_view.update(cx, Self::dialog_closed);
                });
            match rendered {
                // The footer stays in view while the body scrolls.
                Ok((title, body, footer)) => dialog
                    .title(title)
                    .child(body)
                    .when_some(footer, gpui_component::dialog::Dialog::footer),
                Err(_) => dialog
                    .title(app_strong_text("Swap"))
                    .child(app_muted_text("Wallet session ended.")),
            }
        });
        self.dialog = Some(SwapDialog {
            view,
            focus: window.focused(cx).as_ref().map(FocusHandle::downgrade),
            outcome_fees_open: false,
            outcome_details_open: false,
        });
        self.load_detail_destination(view, cx);
        self.focus_view(view, window, cx);
        cx.notify();
    }

    /// Give focus to the view's first control, or to the dialog, so keyboard input and Escape
    /// reach the dialog after its content changed.
    fn focus_view(
        &mut self,
        view: SwapDialogView,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match view {
            SwapDialogView::Form => self.focus_form_amount(window, cx),
            SwapDialogView::Orders => {
                let list = self.orders_list(window, cx);
                list.update(cx, |list, cx| list.focus(window, cx));
            }
            SwapDialogView::Detail(_)
            | SwapDialogView::PastDetail(..)
            | SwapDialogView::CancelledPreparation(_) => {
                if let Some(focus) = self.swap_dialog_focus() {
                    focus.focus(window, cx);
                }
            }
        }
    }

    fn orders_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<ListState<SwapOrdersDelegate>> {
        if let Some(list) = &self.orders_list {
            return list.clone();
        }
        let view = cx.entity().downgrade();
        let list = cx.new(|cx| {
            ListState::new(
                SwapOrdersDelegate {
                    view,
                    rows: Vec::new(),
                    selected: None,
                },
                window,
                cx,
            )
            .selectable(true)
        });
        self.orders_list = Some(list.clone());
        list
    }

    fn dialog_closed(&mut self, cx: &mut Context<'_, Self>) {
        self.dialog = None;
        self.form = None;
        self.reapproval = None;
        self.cancelled = None;
        cx.notify();
    }

    /// Close the swap dialog from one of its own buttons, where it's the topmost dialog.
    pub(super) fn close_swap_dialog(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.dialog_closed(cx);
        window.close_dialog(cx);
    }

    fn go_back(&mut self, target: SwapDialogView, window: &mut Window, cx: &mut Context<'_, Self>) {
        if target == SwapDialogView::Form && self.form.is_none() {
            // Back from My orders sets up a new swap when there is no form to return to.
            let sell = if self.busy() {
                None
            } else {
                self.sell_assets(None, cx).first().map(|asset| asset.token)
            };
            match sell {
                Some(sell) => self.open_new_form(sell, window, cx),
                None => self.close_swap_dialog(window, cx),
            }
            return;
        }
        self.navigate(target, window, cx);
    }

    fn set_orders_filter(
        &mut self,
        filter: Option<SwapOrderGroup>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.orders_filter = filter;
        if let Some(list) = self.orders_list.clone() {
            list.update(cx, |list, cx| {
                list.set_selected_index(None, window, cx);
                list.focus(window, cx);
            });
        }
        cx.notify();
    }

    /// Every recorded swap, newest first by its start. A reused stealth account's earlier swaps
    /// are listed on their own, and so are swaps removed from the Private tab and stopped
    /// setups.
    fn order_entries(&self, cx: &App) -> Vec<OrderEntry<'_>> {
        let mut entries = Vec::new();
        for record in self.records.iter().filter(|record| {
            is_swap_record(record)
                || self.pending_order(record).is_some()
                || super::model::cancelled_swap_uses(record).next().is_some()
        }) {
            let mut has_cancelled = false;
            for claimed in super::model::cancelled_swap_uses(record) {
                has_cancelled = true;
                let swap = SwapIdentity {
                    operation: record.operation(),
                    swap_use: claimed.id(),
                };
                let attention = self
                    .cancelled_use(swap, cx)
                    .is_some_and(|cancelled| cancelled.needs_attention());
                entries.push(OrderEntry {
                    record,
                    view: SwapDialogView::CancelledPreparation(swap),
                    order: None,
                    stage: SwapStage::SetupRetired,
                    group: if attention {
                        SwapOrderGroup::NeedsAttention
                    } else {
                        SwapOrderGroup::Ended
                    },
                    started: claimed.started_at().map(|at| ("Started", at)),
                });
            }
            let pending = self.pending_order(record);
            let orders = record
                .swap()
                .map_or(&[][..], wallet_ops::vault::SwapOperationRecord::orders);
            let (earlier, latest) = self.record_history(record);
            for swap in &earlier {
                let Some(order) = orders.get(swap.orders.end - 1) else {
                    continue;
                };
                // An earlier swap ended before the next one could start.
                let stage = swap_order_stage(record, order);
                entries.push(OrderEntry {
                    record,
                    view: SwapDialogView::PastDetail(
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: swap.swap_use,
                        },
                        swap.orders.start,
                    ),
                    order: Some(order),
                    stage,
                    group: swap_order_group(stage, false, false),
                    started: self.swap_started(record, Some(swap), cx),
                });
            }
            if has_cancelled
                && orders.is_empty()
                && pending.is_none()
                && super::model::prepared_swap_use(record).is_none()
            {
                continue;
            }
            let stage = self.progress_stage(record);
            entries.push(OrderEntry {
                record,
                view: SwapDialogView::Detail(record.operation()),
                order: orders.last().filter(|_| pending.is_none()),
                stage,
                group: swap_order_group(
                    stage,
                    record.is_swap_setup_stopped(),
                    record.is_hidden() && pending.is_none(),
                ),
                started: pending
                    .map(|pending| ("Started", pending.started(record)))
                    .or_else(|| self.swap_started(record, latest.as_ref(), cx)),
            });
        }
        entries.sort_by_key(|entry| {
            std::cmp::Reverse((entry.started.map(|(_, at)| at), entry.record.index()))
        });
        entries
    }

    /// How many swaps My orders lists as open.
    pub(super) fn open_order_count(&self, cx: &App) -> usize {
        self.order_entries(cx)
            .iter()
            .filter(|entry| entry.group == SwapOrderGroup::Open)
            .count()
    }

    /// The record's swaps that have orders, oldest first, as the earlier ones and the latest,
    /// which the record's own detail shows. While a draft prepares another swap on the
    /// account, every swap with an order is an earlier one.
    pub(super) fn record_history(
        &self,
        record: &ExecutorRecord,
    ) -> (Vec<RecordSwap>, Option<RecordSwap>) {
        let mut swaps = record_swaps(record);
        let latest = if self
            .pending_order(record)
            .is_some_and(|pending| pending.starts_new_swap(record))
        {
            None
        } else {
            swaps.pop()
        };
        (swaps, latest)
    }

    /// When a swap started: for a swap that reused its stealth account, when its use claimed
    /// the account. Otherwise its first order's `validTo` less the validity it was signed
    /// with, or the profile's order window for records without one, or for a swap without
    /// orders, when its stealth account was reserved. Without either validity, the first
    /// order's expiry, labeled as such.
    pub(super) fn swap_started(
        &self,
        record: &ExecutorRecord,
        swap: Option<&RecordSwap>,
        cx: &App,
    ) -> Option<(&'static str, u64)> {
        let Some(swap) = swap else {
            return record.created_at().map(|at| ("Started", at));
        };
        if let Some(claimed) = record
            .swap_use(swap.swap_use)
            .filter(|swap_use| !swap_use.is_fresh())
            .and_then(wallet_ops::vault::SwapUseRecord::started_at)
        {
            return Some(("Started", claimed));
        }
        let first = record.swap()?.orders().get(swap.orders.start)?;
        let valid_to = u64::from(first.valid_to());
        if let Some(valid_for) = first.bounds().valid_for_secs {
            return Some(("Started", valid_to.saturating_sub(u64::from(valid_for))));
        }
        Some(
            self.swap_profile(cx)
                .map_or(("Order valid until", valid_to), |profile| {
                    (
                        "Started",
                        valid_to.saturating_sub(profile.valid_to_window().as_secs()),
                    )
                }),
        )
    }

    /// The earlier swap `swap` of its stealth account's record, whose first order is `first`:
    /// the record, the swap's orders, and its last order.
    pub(super) fn past_swap(
        &self,
        swap: SwapIdentity,
        first: usize,
    ) -> Option<(&ExecutorRecord, RecordSwap, &SwapOrderRecord)> {
        let record = self.record(swap.operation)?;
        let (earlier, _) = self.record_history(record);
        let past = earlier
            .into_iter()
            .find(|past| past.swap_use == swap.swap_use && past.orders.start == first)?;
        let order = record.swap()?.orders().get(past.orders.end - 1)?;
        Some((record, past, order))
    }

    /// Display strings for an earlier swap, from its own last order and terms.
    pub(super) fn past_labels(
        &self,
        record: &ExecutorRecord,
        order: &SwapOrderRecord,
        cx: &App,
    ) -> SwapLabels {
        let tokens = order_tokens(record, order).unwrap_or_default();
        self.order_labels(
            record,
            tokens,
            Some(order.bounds().spend_amount()),
            Some(order),
            None,
            cx,
        )
    }

    fn past_title(&self, swap: SwapIdentity, first: usize, cx: &App) -> String {
        self.past_swap(swap, first).map_or_else(
            || "Swap".into(),
            |(record, _, order)| format!("Swap {}", self.past_labels(record, order, cx).pair),
        )
    }

    /// The title row, as wide as the body so its trailing button lines up with the body's
    /// trailing edge and stays clear of the dialog's close button.
    fn render_dialog_title(&self, width: Pixels, cx: &Context<'_, Self>) -> gpui::Div {
        let view = self.dialog.as_ref().map(|dialog| dialog.view);
        let (back, title) = match view {
            Some(SwapDialogView::Form) => (
                self.form_back_target().map(SwapDialogView::Detail),
                self.form_title().to_owned(),
            ),
            Some(SwapDialogView::Orders) => (Some(SwapDialogView::Form), "My orders".to_owned()),
            Some(SwapDialogView::Detail(operation)) => (
                Some(SwapDialogView::Orders),
                self.progress_title(operation, cx),
            ),
            Some(SwapDialogView::PastDetail(swap, first)) => (
                Some(SwapDialogView::Orders),
                self.past_title(swap, first, cx),
            ),
            Some(SwapDialogView::CancelledPreparation(swap)) => (
                Some(SwapDialogView::Orders),
                self.cancelled_use(swap, cx).map_or_else(
                    || "Swap preparation".to_owned(),
                    |cancelled| cancelled.title,
                ),
            ),
            None => (None, "Swap".to_owned()),
        };
        let stage = match view {
            Some(SwapDialogView::Detail(operation)) => self
                .record(operation)
                .map(|record| (self.progress_stage(record), swap_delivery(record))),
            Some(SwapDialogView::PastDetail(swap, first)) => self
                .past_swap(swap, first)
                .map(|(record, _, order)| (swap_order_stage(record, order), order.delivery())),
            _ => None,
        };
        let completed = stage.and_then(|(stage, delivery)| {
            (stage == SwapStage::Order(SwapOrderState::Done)).then_some(delivery)
        });
        div()
            .w(width)
            .min_w_0()
            // A small control's height, whether or not the row holds one, so the title stays
            // centered on the close button.
            .min_h_6()
            .flex()
            .items_center()
            .gap_2()
            .children(back.map(|target| {
                app_button_base("swap-dialog-back")
                    .ghost()
                    .small()
                    .flex_none()
                    .icon(IconName::ArrowLeft)
                    .accessibility_label("Back")
                    .tooltip("Back")
                    .debug_selector(|| "swap-dialog-back".into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.go_back(target, window, cx);
                    }))
            }))
            // Beside a chip, the title only takes its own width, so the chip follows it.
            .child(
                app_strong_text(title)
                    .when(completed.is_none(), gpui::Styled::flex_1)
                    .min_w_0()
                    .truncate(),
            )
            .when_some(completed, |row, delivery| {
                // Completed means back in the private balance, here or on a private Bridge
                // swap's destination network; a Public address swap was delivered to its
                // receiver.
                let label = match delivery {
                    SwapDelivery::Reshield => "Completed",
                    SwapDelivery::Bridge(bridge) if bridge.is_private() => "Completed",
                    SwapDelivery::External { .. } | SwapDelivery::Bridge(_) => "Delivered",
                };
                row.child(
                    app_text(label)
                        .flex_none()
                        .text_xs()
                        .px_2()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().success_active)
                        .text_color(cx.theme().success),
                )
            })
            .when(view == Some(SwapDialogView::Form), |row| {
                row.child(self.render_orders_button(cx))
            })
    }

    /// My orders, labeled with the count of open swaps.
    fn render_orders_button(&self, cx: &Context<'_, Self>) -> gpui_component::button::Button {
        let open = self.open_orders;
        app_button_base("swap-my-orders")
            .outline()
            .small()
            .flex_none()
            .icon(Icon::empty().path(LIST_ICON_PATH))
            .accessibility_label(if open > 0 {
                format!("My orders, {open} open")
            } else {
                "My orders".to_owned()
            })
            .debug_selector(|| "swap-my-orders".into())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_button_label("My orders"))
                    .when(open > 0, |label| label.child(order_count(open))),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.navigate(SwapDialogView::Orders, window, cx);
            }))
    }

    /// The view's body and, when it has one, its footer for the dialog's footer slot.
    fn render_dialog_body(
        &self,
        width: Pixels,
        cx: &mut Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let (body, footer) = match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Form) => self.render_form(cx),
            Some(SwapDialogView::Orders) => self.render_orders(cx),
            Some(SwapDialogView::Detail(operation)) => self.render_detail(operation, cx),
            Some(SwapDialogView::PastDetail(swap, first)) => {
                self.render_past_detail(swap, first, cx)
            }
            Some(SwapDialogView::CancelledPreparation(swap)) => self.render_cancelled_use(swap, cx),
            None => (app_muted_text("Wallet session ended."), None),
        };
        (
            body.w(width).min_w_0(),
            // The dialog leaves a small gap above its footer; this keeps the body's spacing.
            footer.map(|footer| footer.w(width).min_w_0().pt_2()),
        )
    }

    fn render_orders(&self, cx: &mut Context<'_, Self>) -> (gpui::Div, Option<gpui::Div>) {
        let app: &App = cx;
        let entries = self.order_entries(app);
        let count = |group| entries.iter().filter(|entry| entry.group == group).count();
        let (open, recovery, attention) = (
            count(SwapOrderGroup::Open),
            count(SwapOrderGroup::NeedsRecovery),
            count(SwapOrderGroup::NeedsAttention),
        );
        let filter = self.orders_filter;
        let rows = entries
            .iter()
            .filter(|entry| filter.is_none_or(|filter| filter == entry.group))
            .map(|entry| self.order_row(entry, app))
            .collect::<Vec<_>>();
        let no_swaps = entries.is_empty();
        // Counts label only the groups that ask for a look.
        let filters = ButtonGroup::new("swap-orders-filter")
            .outline()
            .compact()
            .children(
                [
                    (None, "All", "all", 0),
                    (Some(SwapOrderGroup::Open), "Open", "open", open),
                    (
                        Some(SwapOrderGroup::NeedsRecovery),
                        "Needs recovery",
                        "recovery",
                        recovery,
                    ),
                    (
                        Some(SwapOrderGroup::NeedsAttention),
                        "Needs attention",
                        "attention",
                        attention,
                    ),
                    (Some(SwapOrderGroup::Ended), "Ended", "ended", 0),
                ]
                .into_iter()
                // Only a bridge deposit needs attention, so its group shows while one does.
                .filter(|&(group, _, _, count)| {
                    group != Some(SwapOrderGroup::NeedsAttention) || count > 0 || filter == group
                })
                .map(|(group, label, id, count)| {
                    app_segment_button(
                        SharedString::from(format!("swap-orders-filter-{id}")),
                        label,
                        filter == group,
                        false,
                        (count > 0).then(|| order_count(count).into_any_element()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_orders_filter(group, window, cx);
                    }))
                }),
            );
        let content = if rows.is_empty() {
            // Opening My orders and choosing a filter focus the list. Without rows the list
            // isn't drawn, so the empty state holds its focus and Escape still reaches the
            // dialog.
            let focus = self
                .orders_list
                .as_ref()
                .map(|list| list.read(cx).focus_handle(cx));
            Self::render_orders_empty(no_swaps, filter, cx)
                .when_some(focus, |empty, focus| empty.track_focus(&focus))
        } else if let Some(list) = self.orders_list.clone() {
            let shown = rows.len().min(ORDERS_VISIBLE_ROWS);
            let scrolls = rows.len() > shown;
            let height = ORDER_ROW_HEIGHT * f32::from(u8::try_from(shown).unwrap_or(u8::MAX));
            list.update(cx, |list, _| list.delegate_mut().rows = rows);
            div()
                .w_full()
                .h(rems(height))
                .child(List::new(&list).scrollbar_visible(scrolls))
        } else {
            div()
        };
        (
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(filters)
                .child(content),
            Some(div().flex().items_center().child(powered_by_cow(cx))),
        )
    }

    fn render_orders_empty(
        no_swaps: bool,
        filter: Option<SwapOrderGroup>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let (title, detail) = if no_swaps {
            (
                "No swaps yet",
                Some(
                    "Swaps you start show up here with their status, including ones that ended. Go back to set up your first swap.",
                ),
            )
        } else {
            let title = match filter {
                Some(SwapOrderGroup::Open) => "No open swaps",
                Some(SwapOrderGroup::NeedsRecovery) => "No swaps need recovery",
                Some(SwapOrderGroup::NeedsAttention) => "No swaps need attention",
                Some(SwapOrderGroup::Ended) => "No ended swaps",
                None => "No swaps yet",
            };
            (title, None)
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .px_6()
            .py_8()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(app_strong_text(title))
            .children(detail.map(|detail| app_muted_text(detail).text_center().whitespace_normal()))
            .when(!no_swaps, |empty| {
                empty.child(
                    app_button("swap-orders-show-all", "Show all")
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_orders_filter(None, window, cx);
                        })),
                )
            })
    }

    fn order_row(&self, entry: &OrderEntry<'_>, cx: &App) -> SwapOrderRow {
        let record = entry.record;
        if let SwapDialogView::CancelledPreparation(swap) = entry.view {
            let cancelled = self.cancelled_use(swap, cx);
            let attention = cancelled
                .as_ref()
                .is_some_and(super::progress::CancelledPreparation::needs_attention);
            return SwapOrderRow {
                view: entry.view,
                title: cancelled.map_or_else(
                    || "Swap preparation".to_owned(),
                    |cancelled| cancelled.title,
                ),
                amount: OrderRowAmount::Muted("Nothing traded"),
                meta: entry
                    .started
                    .map_or_else(String::new, |(_, at)| local_date_time_label(at)),
                status: if attention {
                    "Cancelled · check signed work"
                } else {
                    "Preparation cancelled"
                }
                .to_owned(),
                attention,
                icons: [None, None],
            };
        }
        let latest = matches!(entry.view, SwapDialogView::Detail(_));
        let pending = latest.then(|| self.pending_order(record)).flatten();
        let tokens = pending
            .map(|pending| (pending.sell, pending.buy))
            .or_else(|| {
                entry
                    .order
                    .and_then(|order| order_tokens(record, order))
                    .or_else(|| swap_tokens(record))
            });
        let labels = match entry.order {
            Some(order) if !latest => self.past_labels(record, order, cx),
            _ => self.labels(record, cx),
        };
        let amount = match entry.group {
            SwapOrderGroup::Open => match &labels.bridge {
                // A Bridge swap's minimum is on its destination network.
                Some(bridge) => bridge.minimum.clone(),
                None => tokens
                    .zip(entry.order.map_or_else(
                        || {
                            pending
                                .map(|pending| pending.private_minimum)
                                .or_else(|| swap_private_minimum(record))
                        },
                        |order| Some(order.bounds().private_minimum),
                    ))
                    .map(|((_, buy), minimum)| self.token_amount(buy, minimum, cx)),
            }
            .map_or(OrderRowAmount::None, |minimum| OrderRowAmount::Value {
                value: format!("≥ {minimum}"),
                note: None,
            }),
            SwapOrderGroup::NeedsRecovery => labels
                .bridge
                .as_ref()
                .filter(|_| entry.stage.is_held_on_destination())
                // What the destination stealth account holds.
                .and_then(|bridge| bridge.private.as_ref()?.held.clone())
                .or_else(|| {
                    tokens
                        .zip(entry.order)
                        .filter(|_| !entry.stage.is_held_on_destination())
                        .map(|(tokens, order)| self.stranded_amount(tokens, order, entry.stage, cx))
                })
                .map_or(OrderRowAmount::None, |value| OrderRowAmount::Value {
                    value,
                    note: Some(
                        if entry.stage == SwapStage::Order(SwapOrderState::Refunding) {
                            "refunding"
                        } else {
                            "in stealth account"
                        },
                    ),
                }),
            // What the provider holds.
            SwapOrderGroup::NeedsAttention => tokens
                .zip(entry.order.and_then(bridge_sent_amount))
                .map_or(OrderRowAmount::None, |((_, buy), sent)| {
                    OrderRowAmount::Value {
                        value: self.token_amount(buy, sent, cx),
                        note: Some("deposit failed"),
                    }
                }),
            SwapOrderGroup::Ended => match labels.received.clone() {
                Some(value) if entry.stage == SwapStage::Order(SwapOrderState::Done) => {
                    OrderRowAmount::Value {
                        value,
                        note: Some("received"),
                    }
                }
                _ if entry
                    .order
                    .is_none_or(|order| order.observations().traded.is_none()) =>
                {
                    OrderRowAmount::Muted("Nothing traded")
                }
                _ => OrderRowAmount::None,
            },
        };
        // A Bridge swap is named after the token its receiver gets.
        let bought = labels
            .bridge
            .as_ref()
            .map_or(&labels.buy_symbol, |bridge| &bridge.token);
        let title = if bought.is_empty() {
            labels.sell.clone()
        } else {
            format!("{} → {bought}", labels.sell)
        };
        let delivery = pending
            .map(|pending| pending.delivery)
            .or_else(|| entry.order.map(SwapOrderRecord::delivery))
            .unwrap_or_else(|| swap_delivery(record));
        let started = entry.started.map(|(label, at)| {
            if label == "Started" {
                local_date_time_label(at)
            } else {
                format!("valid until {}", local_date_time_label(at))
            }
        });
        // A Public address swap names its receiver, and a Bridge swap also its network and
        // provider. Private swaps return to the private balance, which a private Bridge swap
        // names on its destination network.
        let meta = [
            started,
            Some(format!("#{}", record.index())),
            labels
                .receiver
                .as_ref()
                .map(|receiver| format!("to {receiver}")),
            labels.bridge.as_ref().map(|bridge| {
                format!(
                    "to {} on {} · {}",
                    if bridge.private.is_some() {
                        "private balance"
                    } else {
                        bridge.receiver.as_str()
                    },
                    bridge.network,
                    provider_name(bridge.provider)
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let stopped = latest && record.is_swap_setup_stopped();
        SwapOrderRow {
            view: entry.view,
            title,
            amount,
            meta,
            status: swap_order_status(entry.stage, stopped, &labels),
            attention: matches!(
                entry.group,
                SwapOrderGroup::NeedsRecovery | SwapOrderGroup::NeedsAttention
            ),
            icons: tokens.map_or([None, None], |(sell, buy)| {
                [
                    self.token_icon(sell, cx),
                    match delivery {
                        SwapDelivery::Bridge(bridge) => self
                            .chain_token_metadata(
                                bridge.destination_chain,
                                self.delivered_token(bridge, cx),
                                cx,
                            )
                            .and_then(|metadata| metadata.icon_path),
                        _ => self.token_icon(buy, cx),
                    },
                ]
            }),
        }
    }

    /// My orders' rows as their second line and status, newest first.
    #[cfg(test)]
    pub(super) fn order_rows_for_test(&self, cx: &App) -> Vec<(String, String)> {
        self.order_entries(cx)
            .iter()
            .map(|entry| {
                let row = self.order_row(entry, cx);
                (row.meta, row.status)
            })
            .collect()
    }

    /// What waits in the stealth account after `order`: the unshielded sell amount before a
    /// trade, the traded buy amount after one, and what a bridge refunds. The symbol alone when
    /// the amount wasn't recorded.
    fn stranded_amount(
        &self,
        (sell, buy): (Address, Address),
        order: &SwapOrderRecord,
        stage: SwapStage,
        cx: &App,
    ) -> String {
        let (token, amount) = match stage {
            SwapStage::Order(SwapOrderState::NotDelivered) => (
                buy,
                order
                    .observations()
                    .trade_amounts
                    .map(|trade| trade.buy_amount),
            ),
            SwapStage::Order(SwapOrderState::Refunding) => (buy, bridge_sent_amount(order)),
            _ => (sell, Some(order.bounds().sell_amount)),
        };
        amount.map_or_else(
            || self.token_symbol(token, cx),
            |amount| self.token_amount(token, amount, cx),
        )
    }
}

/// The sell and buy tokens of one of the record's orders, from its own terms.
fn order_tokens(record: &ExecutorRecord, order: &SwapOrderRecord) -> Option<(Address, Address)> {
    let terms = record.swap()?.order_terms(order);
    Some((terms.sell_token(), terms.buy_token()))
}

/// A stable key for a row's element and debug selector.
fn row_key(view: SwapDialogView) -> String {
    match view {
        // One use holds one swap, except orders from before swap uses, which the first
        // order tells apart.
        SwapDialogView::PastDetail(swap, first) => {
            format!(
                "{}-{}-{first}",
                swap.operation.opaque_id(),
                swap.swap_use.opaque_id()
            )
        }
        SwapDialogView::CancelledPreparation(swap) => format!(
            "{}-{}-cancelled",
            swap.operation.opaque_id(),
            swap.swap_use.opaque_id()
        ),
        SwapDialogView::Detail(operation) => operation.opaque_id(),
        SwapDialogView::Form | SwapDialogView::Orders => String::new(),
    }
}

fn render_order_row(row: &SwapOrderRow) -> gpui::Div {
    // The icon slots keep their width, so titles stay on one spine without icons.
    let icons = div()
        .w(rems(2.25))
        .flex_none()
        .flex()
        .items_center()
        .children(row.icons.iter().enumerate().map(|(index, icon)| {
            div()
                .size_5()
                .flex_none()
                .rounded_full()
                .bg(rgb(theme::SURFACE_HOVER))
                .when(index > 0, |slot| slot.ml(rems(-0.5)))
                .children(icon.clone().map(|icon| img(icon).size_5().rounded_full()))
        }));
    let amount = match &row.amount {
        OrderRowAmount::None => None,
        OrderRowAmount::Value { value, note } => Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(app_text(value.clone()))
                .children(note.map(app_muted_text)),
        ),
        OrderRowAmount::Muted(text) => Some(app_muted_text(*text).flex_none()),
    };
    let status = if row.attention {
        Tag::warning()
    } else {
        Tag::secondary()
    };
    let key = row_key(row.view);
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .debug_selector(move || format!("swap-order-row-{key}"))
        .child(icons)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(app_text(row.title.clone()).min_w_0().truncate())
                        .children(amount),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(app_muted_text(row.meta.clone()).min_w_0().truncate())
                        .child(
                            status
                                .outline()
                                .small()
                                .rounded_full()
                                .line_height(gpui::relative(theme::APP_TEXT_LINE_HEIGHT))
                                .child(row.status.clone()),
                        ),
                ),
        )
        .child(
            Icon::new(IconName::ChevronRight)
                .small()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
}

fn order_count(count: usize) -> gpui::Div {
    app_button_label(count.to_string()).text_color(rgb(theme::TEXT_MUTED))
}

/// The credit on the form and My orders: "Powered by" and the unaltered light `CoW` Protocol
/// lockup. It doesn't link anywhere.
pub(super) fn powered_by_cow(cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_2()
        .child(app_text("Powered by").text_color(cx.theme().muted_foreground))
        .child(
            // The lockup's own aspect ratio, 1630 by 400.
            img(COW_PROTOCOL_LOGO_LIGHT_PATH)
                .h(px(26.0))
                .w(px(106.0))
                .flex_none()
                .object_fit(gpui::ObjectFit::Contain),
        )
}

/// The detail's Settled by value: the unaltered light `CoW` head and the protocol's name.
pub(super) fn settled_by_cow() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            img(COW_DAO_LIGHT_PATH)
                .size(px(16.0))
                .flex_none()
                .object_fit(gpui::ObjectFit::Contain),
        )
        .child(app_text("CoW Protocol"))
}
