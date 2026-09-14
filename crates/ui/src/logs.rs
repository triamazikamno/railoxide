use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AppContext, Context, DragMoveEvent, InteractiveElement, IntoElement, ListAlignment, ListOffset,
    ListState, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, list, px, rgb,
};
use gpui_component::scroll::Scrollbar;
use parking_lot::RwLock;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::registry::LookupSpan;
use url::Url;

use crate::theme;

/// Maximum number of retained log lines kept in the bounded in-memory ring.
pub const DEFAULT_LOG_CAPACITY: usize = 100;

const LOG_PANE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const LOG_HEADER_HEIGHT: Pixels = px(32.0);
const LOG_ROW_MIN_HEIGHT: Pixels = px(32.0);
const LOG_CELL_PADDING_X: Pixels = px(8.0);
const LOG_CELL_PADDING_Y: Pixels = px(5.0);
const LOG_SCROLLBAR_WIDTH: Pixels = px(16.0);
const LOG_RESIZE_HANDLE_WIDTH: Pixels = px(10.0);
const LOG_LIST_OVERDRAW: Pixels = px(480.0);

const DEFAULT_TIME_WIDTH: Pixels = px(100.0);
const DEFAULT_LEVEL_WIDTH: Pixels = px(50.0);
const DEFAULT_TARGET_WIDTH: Pixels = px(200.0);

const MIN_TIME_WIDTH: Pixels = px(72.0);
const MIN_LEVEL_WIDTH: Pixels = px(48.0);
const MIN_TARGET_WIDTH: Pixels = px(80.0);
const MIN_MESSAGE_WIDTH: Pixels = px(140.0);

const REDACTED_URL_SCHEMES: &[&str] = &[
    "http://",
    "https://",
    "socks5://",
    "socks5h://",
    "ws://",
    "wss://",
];

/// Single retained log line from the app's in-memory tracing layer.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: u64,
    pub unix_ms: u64,
    pub level: Level,
    pub target: Arc<str>,
    pub message: Arc<str>,
}

struct LogState {
    logs: VecDeque<LogEntry>,
    capacity: usize,
    seq: AtomicU64,
    rev: AtomicU64,
}

impl LogState {
    fn new(capacity: usize) -> Self {
        Self {
            logs: VecDeque::with_capacity(capacity),
            capacity,
            seq: AtomicU64::new(0),
            rev: AtomicU64::new(0),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn push(&mut self, entry: LogEntry) {
        if self.logs.len() == self.capacity {
            self.logs.pop_front();
        }
        self.logs.push_back(entry);
        self.rev.fetch_add(1, Ordering::Release);
    }
}

/// Shared app-wide log store suitable for a hidden diagnostics drawer or tab.
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<RwLock<LogState>>,
}

impl LogStore {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LogState::new(capacity))),
        }
    }

    #[must_use]
    pub fn rev(&self) -> u64 {
        self.inner.read().rev.load(Ordering::Acquire)
    }

    pub fn push(&self, mut entry: LogEntry) {
        let mut state = self.inner.write();
        entry.seq = state.next_seq();
        state.push(entry);
    }

    #[must_use]
    pub fn logs(&self) -> Vec<LogEntry> {
        self.inner.read().logs.iter().cloned().collect()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.read().capacity
    }
}

/// Bounded in-memory tracing layer that mirrors app logs into [`LogStore`].
pub struct UiLogLayer {
    logs: LogStore,
}

impl UiLogLayer {
    #[must_use]
    pub const fn new(logs: LogStore) -> Self {
        Self { logs }
    }
}

impl<S> Layer<S> for UiLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        self.logs.push(LogEntry {
            seq: 0,
            unix_ms: now_unix_ms(),
            level: *metadata.level(),
            target: Arc::from(metadata.target()),
            message: Arc::from(visitor.into_message()),
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    extras: String,
}

impl MessageVisitor {
    fn into_message(mut self) -> String {
        if !self.extras.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(&self.extras);
        }
        self.message
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        let value = redact_log_urls(value);
        if field.name() == "message" {
            self.message.push_str(&value);
        } else {
            if !self.extras.is_empty() {
                self.extras.push(' ');
            }
            let _ = write!(&mut self.extras, "{}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let value = redact_log_urls(&format!("{value:?}"));
        if field.name() == "message" {
            self.message.push_str(&value);
        } else {
            if !self.extras.is_empty() {
                self.extras.push(' ');
            }
            let _ = write!(&mut self.extras, "{}={value}", field.name());
        }
    }
}

fn redact_log_urls(value: &str) -> String {
    let Some(mut offset) = next_url_start(value) else {
        return value.to_string();
    };

    let mut redacted = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        redacted.push_str(&rest[..offset]);
        let candidate = &rest[offset..];
        let end = url_candidate_end(candidate);
        let url_text = &candidate[..end];
        if let Ok(url) = Url::parse(url_text) {
            redacted.push_str(&redact_url_for_log(&url));
            rest = &candidate[end..];
        } else {
            let mut chars = candidate.chars();
            if let Some(ch) = chars.next() {
                redacted.push(ch);
                rest = chars.as_str();
            } else {
                rest = "";
            }
        }

        let Some(next_offset) = next_url_start(rest) else {
            redacted.push_str(rest);
            return redacted;
        };
        offset = next_offset;
    }
}

fn next_url_start(value: &str) -> Option<usize> {
    REDACTED_URL_SCHEMES
        .iter()
        .filter_map(|scheme| value.find(scheme))
        .min()
}

fn url_candidate_end(value: &str) -> usize {
    value
        .char_indices()
        .find_map(|(index, ch)| {
            (ch.is_whitespace()
                || matches!(ch, '"' | '\'' | ')' | ']' | '}' | '<' | '>' | ',' | ';'))
            .then_some(index)
        })
        .unwrap_or(value.len())
}

fn redact_url_for_log(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    if !redacted.cannot_be_a_base() {
        redacted.set_path("");
    }
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

#[must_use]
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug, Clone, Copy)]
struct LogColumnWidths {
    time: Pixels,
    level: Pixels,
    target: Pixels,
}

impl Default for LogColumnWidths {
    fn default() -> Self {
        Self {
            time: DEFAULT_TIME_WIDTH,
            level: DEFAULT_LEVEL_WIDTH,
            target: DEFAULT_TARGET_WIDTH,
        }
    }
}

impl LogColumnWidths {
    fn fixed_width(self) -> Pixels {
        self.time + self.level + self.target
    }

    const fn width(self, column: LogResizeColumn) -> Pixels {
        match column {
            LogResizeColumn::Time => self.time,
            LogResizeColumn::Level => self.level,
            LogResizeColumn::Target => self.target,
        }
    }

    const fn set_width(&mut self, column: LogResizeColumn, width: Pixels) {
        match column {
            LogResizeColumn::Time => self.time = width,
            LogResizeColumn::Level => self.level = width,
            LogResizeColumn::Target => self.target = width,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogResizeColumn {
    Time,
    Level,
    Target,
}

impl LogResizeColumn {
    const fn index(self) -> usize {
        match self {
            Self::Time => 0,
            Self::Level => 1,
            Self::Target => 2,
        }
    }

    const fn min_width(self) -> Pixels {
        match self {
            Self::Time => MIN_TIME_WIDTH,
            Self::Level => MIN_LEVEL_WIDTH,
            Self::Target => MIN_TARGET_WIDTH,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LogResizeState {
    column: LogResizeColumn,
    start_x: Pixels,
    start_width: Pixels,
}

struct LogResizeDrag;

impl Render for LogResizeDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size(px(1.0))
    }
}

fn format_time(unix_ms: u64) -> String {
    let secs = unix_ms / 1_000;
    let ms = unix_ms % 1_000;
    let hh = (secs / 3_600) % 24;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    format!("{hh:02}:{mm:02}:{ss:02}.{ms:03}")
}

/// Reusable app-wide logs pane. Hosts a variable-height list and mirrors
/// [`LogStore`] changes without tying logs to any specific feature pane.
pub struct LogsPane {
    logs: LogStore,
    rows: Arc<[LogEntry]>,
    list_state: ListState,
    column_widths: LogColumnWidths,
    last_width: Option<Pixels>,
    follow_tail: bool,
    resizing: Option<LogResizeState>,
}

impl LogsPane {
    pub fn new(logs: LogStore, _window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let rows: Arc<[LogEntry]> = Arc::from(logs.logs());
        let list_state = ListState::new(rows.len(), ListAlignment::Bottom, LOG_LIST_OVERDRAW);
        let entity = cx.entity();
        list_state.set_scroll_handler(move |event, _window, cx| {
            entity.update(cx, |pane, cx| {
                pane.follow_tail = should_follow_tail(event.is_scrolled);
                cx.notify();
            });
        });

        let refresh_logs = logs.clone();
        cx.spawn(async move |this, cx| {
            let mut last_rev = refresh_logs.rev();
            loop {
                cx.background_executor()
                    .timer(LOG_PANE_REFRESH_INTERVAL)
                    .await;
                let current_rev = refresh_logs.rev();
                if current_rev == last_rev {
                    continue;
                }
                last_rev = current_rev;
                let rows = refresh_logs.logs();

                if this
                    .update(cx, |pane, cx| {
                        pane.set_rows(rows);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            logs,
            rows,
            list_state,
            column_widths: LogColumnWidths::default(),
            last_width: None,
            follow_tail: true,
            resizing: None,
        }
    }

    fn sync_pane_width(&mut self, width: Pixels, cx: &mut Context<'_, Self>) {
        if self.last_width == Some(width) {
            return;
        }

        self.last_width = Some(width);
        self.invalidate_row_measurements();
        cx.notify();
    }

    fn set_rows(&mut self, rows: Vec<LogEntry>) {
        let rows = Arc::<[LogEntry]>::from(rows);
        self.sync_list_items(&rows);
        self.rows = rows;
    }

    fn sync_list_items(&self, new_rows: &[LogEntry]) {
        let old_count = self.rows.len();
        let new_count = new_rows.len();

        if old_count == 0 {
            self.list_state.splice(0..0, new_count);
            return;
        }

        if new_count == 0 {
            self.replace_all_list_items(0);
            return;
        }

        let Some(old_last_seq) = self.rows.last().map(|entry| entry.seq) else {
            self.replace_all_list_items(new_count);
            return;
        };
        let Some(new_first_seq) = new_rows.first().map(|entry| entry.seq) else {
            self.replace_all_list_items(new_count);
            return;
        };

        let dropped = self
            .rows
            .iter()
            .take_while(|entry| entry.seq < new_first_seq)
            .count();
        let added = new_rows
            .iter()
            .filter(|entry| entry.seq > old_last_seq)
            .count();
        let old_overlap = old_count.saturating_sub(dropped);
        let new_overlap = new_count.saturating_sub(added);
        let overlap_matches = old_overlap == new_overlap
            && self.rows[dropped..]
                .iter()
                .zip(new_rows[..new_overlap].iter())
                .all(|(old, new)| old.seq == new.seq);

        if !overlap_matches {
            self.replace_all_list_items(new_count);
            return;
        }

        if dropped > 0 {
            self.list_state.splice(0..dropped, 0);
        }

        if added > 0 {
            let append_at = old_count.saturating_sub(dropped);
            self.list_state.splice(append_at..append_at, added);
        }

        if self.list_state.item_count() != new_count {
            self.replace_all_list_items(new_count);
        }
    }

    fn replace_all_list_items(&self, new_count: usize) {
        let old_count = self.list_state.item_count();
        let scroll_top = self.list_state.logical_scroll_top();
        self.list_state.splice(0..old_count, new_count);
        if !self.follow_tail {
            self.list_state
                .scroll_to(clamp_scroll_top(scroll_top, new_count));
        }
    }

    fn invalidate_row_measurements(&self) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }

        let scroll_top = self.list_state.logical_scroll_top();
        self.list_state.splice(0..count, count);
        if !self.follow_tail {
            self.list_state
                .scroll_to(clamp_scroll_top(scroll_top, count));
        }
    }

    const fn begin_resize(&mut self, column: LogResizeColumn, start_x: Pixels) {
        self.resizing = Some(LogResizeState {
            column,
            start_x,
            start_width: self.column_widths.width(column),
        });
    }

    fn update_resize(&mut self, pointer_x: Pixels) {
        let Some(resizing) = self.resizing else {
            return;
        };

        let width = resizing.start_width + pointer_x - resizing.start_x;
        let width = self.clamp_column_width(resizing.column, width);
        if self.column_widths.width(resizing.column) == width {
            return;
        }

        self.column_widths.set_width(resizing.column, width);
        self.invalidate_row_measurements();
    }

    const fn end_resize(&mut self) {
        self.resizing = None;
    }

    fn clamp_column_width(&self, column: LogResizeColumn, width: Pixels) -> Pixels {
        let min = column.min_width();
        let pane_width = self.last_width.unwrap_or(px(1_280.0)) - LOG_SCROLLBAR_WIDTH;
        let other_fixed = self.column_widths.fixed_width() - self.column_widths.width(column);
        let max = (pane_width - other_fixed - MIN_MESSAGE_WIDTH).max(min);
        width.max(min).min(max)
    }

    fn render_header(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        div()
            .h(LOG_HEADER_HEIGHT)
            .w_full()
            .pr(LOG_SCROLLBAR_WIDTH)
            .flex()
            .items_start()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(self.render_header_cell(
                "time",
                self.column_widths.time,
                Some(LogResizeColumn::Time),
                cx,
            ))
            .child(self.render_header_cell(
                "level",
                self.column_widths.level,
                Some(LogResizeColumn::Level),
                cx,
            ))
            .child(self.render_header_cell(
                "target",
                self.column_widths.target,
                Some(LogResizeColumn::Target),
                cx,
            ))
            .child(Self::render_message_header_cell("message"))
    }

    fn render_header_cell(
        &self,
        label: &'static str,
        width: Pixels,
        resize_column: Option<LogResizeColumn>,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let mut cell = div()
            .relative()
            .h_full()
            .w(width)
            .flex_none()
            .flex()
            .items_center()
            .px(LOG_CELL_PADDING_X)
            .text_color(rgb(theme::TEXT_MUTED))
            .font_weight(gpui::FontWeight::MEDIUM)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(SharedString::from(label));

        if let Some(column) = resize_column {
            let active = self.resizing.is_some_and(|state| state.column == column);
            cell = cell.child(Self::render_resize_handle(column, active, cx));
        }

        cell
    }

    fn render_message_header_cell(label: &'static str) -> impl IntoElement {
        div()
            .relative()
            .h_full()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .px(LOG_CELL_PADDING_X)
            .text_color(rgb(theme::TEXT_MUTED))
            .font_weight(gpui::FontWeight::MEDIUM)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(SharedString::from(label))
    }

    fn render_resize_handle(
        column: LogResizeColumn,
        active: bool,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let divider_color = if active {
            rgb(theme::ACCENT_BLUE)
        } else {
            rgb(theme::BORDER)
        };

        div()
            .id(("log-column-resize", column.index()))
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(LOG_RESIZE_HANDLE_WIDTH)
            .cursor_col_resize()
            .occlude()
            .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    this.begin_resize(column, event.position.x);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    this.end_resize();
                    cx.notify();
                }),
            )
            .on_drag(column, |_column, _position, _window, cx| {
                cx.stop_propagation();
                cx.new(|_| LogResizeDrag)
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<LogResizeColumn>, _window, cx| {
                    let column = *event.drag(cx);
                    if this.resizing.is_none_or(|state| state.column != column) {
                        this.begin_resize(column, event.event.position.x);
                    }
                    this.update_resize(event.event.position.x);
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(1.0))
                    .bg(divider_color),
            )
    }

    fn render_body(&self) -> impl IntoElement {
        let rows = self.rows.clone();
        let row_count = rows.len();
        let widths = self.column_widths;
        let content = if row_count == 0 {
            render_empty_logs().into_any_element()
        } else {
            list(self.list_state.clone(), move |ix, _window, _cx| {
                rows.get(ix).map_or_else(
                    || div().into_any_element(),
                    |entry| render_log_row(entry, widths, ix),
                )
            })
            .size_full()
            .into_any_element()
        };

        let mut body = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .pr(LOG_SCROLLBAR_WIDTH)
            .overflow_hidden()
            .child(content);

        if row_count > 0 {
            body = body.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(LOG_SCROLLBAR_WIDTH)
                    .child(Scrollbar::vertical(&self.list_state)),
            );
        }

        body
    }

    #[must_use]
    pub fn retained_count(&self) -> usize {
        self.logs.logs().len()
    }
}

fn clamp_scroll_top(scroll_top: ListOffset, count: usize) -> ListOffset {
    let item_ix = scroll_top.item_ix.min(count);
    ListOffset {
        item_ix,
        offset_in_item: if item_ix == count {
            px(0.0)
        } else {
            scroll_top.offset_in_item
        },
    }
}

const fn should_follow_tail(is_scrolled: bool) -> bool {
    !is_scrolled
}

fn render_log_row(entry: &LogEntry, widths: LogColumnWidths, row_ix: usize) -> gpui::AnyElement {
    let (level_label, level_color) = level_style(entry.level);
    let bg = if row_ix.is_multiple_of(2) {
        rgb(theme::SURFACE_ELEVATED)
    } else {
        rgb(theme::SURFACE_ELEVATED_ALT)
    };

    div()
        .w_full()
        .min_h(LOG_ROW_MIN_HEIGHT)
        .flex()
        .items_start()
        .bg(bg)
        .border_b_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(render_nowrap_log_cell(
            SharedString::from(format_time(entry.unix_ms)),
            widths.time,
            rgb(theme::TEXT_SUBTLE),
        ))
        .child(render_nowrap_log_cell(
            SharedString::from(level_label),
            widths.level,
            level_color,
        ))
        .child(render_nowrap_log_cell(
            SharedString::from(entry.target.as_ref().to_owned()),
            widths.target,
            rgb(theme::PURPLE),
        ))
        .child(
            div()
                .flex_1()
                .min_w(MIN_MESSAGE_WIDTH)
                .px(LOG_CELL_PADDING_X)
                .py(LOG_CELL_PADDING_Y)
                .text_color(rgb(theme::TEXT))
                .whitespace_normal()
                .child(SharedString::from(entry.message.as_ref().to_owned())),
        )
        .into_any_element()
}

fn render_nowrap_log_cell(text: SharedString, width: Pixels, color: Rgba) -> impl IntoElement {
    div()
        .w(width)
        .flex_none()
        .px(LOG_CELL_PADDING_X)
        .py(LOG_CELL_PADDING_Y)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(color)
        .child(text)
}

fn render_empty_logs() -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child("No log entries yet")
}

fn level_style(level: Level) -> (&'static str, Rgba) {
    match level {
        Level::ERROR => ("ERR", rgb(theme::DANGER)),
        Level::WARN => ("WRN", rgb(theme::WARNING)),
        Level::INFO => ("INF", rgb(theme::SUCCESS)),
        Level::DEBUG => ("DBG", rgb(theme::INFO)),
        Level::TRACE => ("TRC", rgb(theme::TEXT_MUTED)),
    }
}

impl Render for LogsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let entity = cx.entity();

        div()
            .relative()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(self.render_header(cx))
            .child(self.render_body())
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.sync_pane_width(bounds.size.width, cx);
                        });
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn default_log_capacity_is_100() {
        assert_eq!(DEFAULT_LOG_CAPACITY, 100);
    }

    #[test]
    fn bounded_retention_drops_oldest() {
        let logs = LogStore::new(4);
        let layer = UiLogLayer::new(logs.clone());
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..10 {
                tracing::info!(iter = i, "log-line");
            }
        });
        let entries = logs.logs();
        assert_eq!(entries.len(), 4, "only the newest 4 entries should remain");
        assert!(entries.last().unwrap().message.contains("log-line"));
    }

    #[test]
    fn follow_tail_stays_enabled_at_bottom() {
        assert!(should_follow_tail(false));
    }

    #[test]
    fn follow_tail_pauses_when_scrolled_up() {
        assert!(!should_follow_tail(true));
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let logs = LogStore::new(16);
        let layer = UiLogLayer::new(logs.clone());
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..5 {
                tracing::info!("tick");
            }
        });
        let seqs: Vec<u64> = logs.logs().iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn ui_log_layer_redacts_url_credentials_paths_queries_and_fragments() {
        let logs = LogStore::new(4);
        let layer = UiLogLayer::new(logs.clone());
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                rpc = "https://user:pass@example.com/private/project-id?token=secret#fragment",
                proxy_url = "socks5h://proxy-user:proxy-pass@proxy.example:9050",
                "request failed at https://api-user:api-pass@api.example/rpc/key?api_key=secret#fragment"
            );
        });

        let entry = logs.logs().pop().expect("log entry");
        assert!(entry.message.contains("https://example.com/"));
        assert!(entry.message.contains("socks5h://proxy.example:9050"));
        assert!(entry.message.contains("https://api.example/"));
        assert!(!entry.message.contains("user"));
        assert!(!entry.message.contains("pass"));
        assert!(!entry.message.contains("project-id"));
        assert!(!entry.message.contains("secret"));
        assert!(!entry.message.contains("fragment"));
    }
}
