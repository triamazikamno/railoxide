use std::borrow::Cow;
use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat};
use gpui::{
    AnyElement, App, AppContext, Context, DragMoveEvent, EventEmitter, FocusHandle, HighlightStyle,
    InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, Rgba, SharedString, StatefulInteractiveElement,
    Styled, StyledText, WeakEntity, Window, canvas, div, list, prelude::FluentBuilder as _, px,
    rgb,
};
use gpui_component::{
    Sizable as _,
    button::ButtonVariants as _,
    empty::{Empty, EmptyContent, EmptyHeader, EmptyTitle},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    scroll::Scrollbar,
};
use parking_lot::RwLock;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::registry::LookupSpan;
use url::Url;

use crate::clipboard::copy_to_clipboard_with_custom_toast;
use crate::controls::app_button;
use crate::theme;

/// Maximum number of retained log lines kept in the bounded in-memory ring.
pub const DEFAULT_LOG_CAPACITY: usize = 5_000;

/// Key context of [`LogsPane`]. The wallet binds the pane's actions in it.
pub const LOGS_PANE_KEY_CONTEXT: &str = "LogsPane";

/// Moves the cursor to the previous shown line and selects only it.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct SelectPreviousLine;

/// Moves the cursor to the next shown line and selects only it.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct SelectNextLine;

/// Moves the cursor up and selects the shown lines from the anchor to it.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct ExtendSelectionUp;

/// Moves the cursor down and selects the shown lines from the anchor to it.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct ExtendSelectionDown;

/// Selects every shown line.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct SelectAllShownLines;

/// Copies the selected lines in the [`format_log_lines`] format.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct CopySelectedLines;

/// Clears the selection; propagates when nothing is selected.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct ClearLineSelection;

/// Asks the owner to focus the logs search ([`LogsPaneEvent::FocusSearch`]).
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(namespace = logs, no_json)]
pub struct FocusLogsSearch;

/// Requests from [`LogsPane`] to the owner of the logs search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogsPaneEvent {
    /// Focus the search field.
    FocusSearch,
    /// Replace every `target:` chip with one for this target.
    ShowOnlyTarget(Arc<str>),
    /// Add a `hide:` chip for this target.
    HideTarget(Arc<str>),
    /// Empty the search field.
    ClearSearch,
}

const LOG_PANE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const LOG_HEADER_HEIGHT: Pixels = px(32.0);
const LOG_ROW_MIN_HEIGHT: Pixels = px(32.0);
const LOG_CELL_PADDING_X: Pixels = px(8.0);
const LOG_CELL_PADDING_Y: Pixels = px(5.0);
const LOG_SCROLLBAR_WIDTH: Pixels = px(16.0);
const LOG_RESIZE_HANDLE_WIDTH: Pixels = px(10.0);
const LOG_LIST_OVERDRAW: Pixels = px(480.0);
const LOG_NEW_LINES_INSET: Pixels = px(8.0);

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

    /// Drops every retained line. `seq` keeps counting, so later lines never reuse a number.
    pub fn clear(&self) {
        let mut state = self.inner.write();
        state.logs.clear();
        state.rev.fetch_add(1, Ordering::Release);
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

/// Which retained lines [`LogsPane`] shows. The wallet's logs search builds it from its chips
/// and free text; an empty filter shows every line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Free text, trimmed and lowercased. A line passes when its message or target contains it.
    pub text: String,
    /// Least severe level shown: a line passes at this level or more severe. Several `level:`
    /// chips each pass their level and above, so the caller passes the most verbose of them.
    pub min_level: Option<Level>,
    /// `target:` modules; a line passes when any matches its target ([`target_matches`]).
    /// Empty passes every line.
    pub targets: Vec<String>,
    /// `hide:` modules; a line passes only when none matches its target.
    pub hides: Vec<String>,
}

impl LogFilter {
    /// Builds a filter, trimming and lowercasing `text`.
    #[must_use]
    pub fn new(
        text: &str,
        min_level: Option<Level>,
        targets: Vec<String>,
        hides: Vec<String>,
    ) -> Self {
        Self {
            text: text.trim().to_lowercase(),
            min_level,
            targets,
            hides,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.min_level.is_none()
            && self.targets.is_empty()
            && self.hides.is_empty()
    }

    #[must_use]
    pub fn matches(&self, entry: &LogEntry) -> bool {
        let target = entry.target.as_ref();
        self.min_level.is_none_or(|min| entry.level <= min)
            && (self.targets.is_empty()
                || self
                    .targets
                    .iter()
                    .any(|module| target_matches(target, module)))
            && !self
                .hides
                .iter()
                .any(|module| target_matches(target, module))
            && (self.text.is_empty()
                || entry.message.to_lowercase().contains(&self.text)
                || target.to_lowercase().contains(&self.text))
    }
}

/// Whether `target` is `module` or one of its submodules: `wallet_ops::rpc_broker` matches
/// `wallet_ops::rpc_broker::warmup` but not `wallet_ops::rpc_broker_metrics`.
#[must_use]
pub fn target_matches(target: &str, module: &str) -> bool {
    target
        .strip_prefix(module)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

/// Copied-text form of `entries`, one per line in input order:
/// `2026-11-05T14:03:22.123Z WARN wallet_ops::rpc_broker: message`.
#[must_use]
pub fn format_log_lines(entries: &[LogEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let time = i64::try_from(entry.unix_ms)
                .ok()
                .and_then(DateTime::from_timestamp_millis)
                .unwrap_or_default()
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            format!(
                "{time} {} {}: {}",
                entry.level.as_str(),
                entry.target,
                entry.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The messages of `entries`, one per line in input order.
#[must_use]
pub fn format_log_messages(entries: &[LogEntry]) -> String {
    entries
        .iter()
        .map(|entry| entry.message.as_ref())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `n` with thousands separators, such as `4,812`.
#[must_use]
pub fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

const fn lines_word(n: usize) -> &'static str {
    if n == 1 { "line" } else { "lines" }
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
        let value = redact_log_urls(&compact_log_value(value));
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
        let value = redact_log_urls(&compact_log_value(&format!("{value:?}")));
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

/// Longest log field value kept whole.
const MAX_LOG_FIELD_LEN: usize = 512;

/// Cuts an HTML page, or whatever runs past [`MAX_LOG_FIELD_LEN`] bytes, out of a log field
/// value. Errors such as Alloy's HTTP error carry whole response bodies: worth keeping as short
/// JSON, noise as an HTML error page.
#[must_use]
pub fn compact_log_value(value: &str) -> Cow<'_, str> {
    let html_start = ["<!doctype html", "<html"]
        .iter()
        .filter_map(|marker| {
            value
                .as_bytes()
                .windows(marker.len())
                .position(|window| window.eq_ignore_ascii_case(marker.as_bytes()))
        })
        .min();
    if html_start.is_none() && value.len() <= MAX_LOG_FIELD_LEN {
        return Cow::Borrowed(value);
    }
    let cut = value.floor_char_boundary(html_start.unwrap_or(value.len()).min(MAX_LOG_FIELD_LEN));
    // Cutting inside a URL would leave credentials that URL redaction can no longer parse.
    let cut = split_url_start(value, cut).unwrap_or(cut);
    let kind = if html_start.is_some() { "HTML, " } else { "" };
    Cow::Owned(format!(
        "{} …[{kind}{} bytes omitted]",
        value[..cut].trim_end(),
        value.len() - cut
    ))
}

/// Start of the URL that a cut at `cut` would split.
fn split_url_start(value: &str, cut: usize) -> Option<usize> {
    let start = REDACTED_URL_SCHEMES
        .iter()
        .filter_map(|scheme| value[..cut].rfind(scheme))
        .max()?;
    (start + url_candidate_end(&value[start..]) > cut).then_some(start)
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

/// Row selection keyed by `seq`, since row indices shift as the ring drops lines and the filter
/// changes.
#[derive(Debug, Default)]
struct LogSelection {
    selected: BTreeSet<u64>,
    anchor: Option<u64>,
    cursor: Option<u64>,
}

impl LogSelection {
    fn select_only(&mut self, seq: u64) {
        self.selected = BTreeSet::from([seq]);
        self.anchor = Some(seq);
        self.cursor = Some(seq);
    }

    /// Pointer press on the shown row `seq`: Shift (`extend`) selects from the anchor, the
    /// secondary modifier (`toggle`) flips the row, and a plain press selects only it.
    fn click(&mut self, rows: &[LogEntry], seq: u64, extend: bool, toggle: bool) {
        if toggle {
            if !self.selected.remove(&seq) {
                self.selected.insert(seq);
            }
            self.anchor = Some(seq);
            self.cursor = Some(seq);
        } else if extend && let Some(anchor) = self.anchor {
            self.cursor = Some(seq);
            self.select_range(rows, anchor, seq);
        } else {
            self.select_only(seq);
        }
    }

    fn select_range(&mut self, rows: &[LogEntry], from: u64, to: u64) {
        let (Some(from_ix), Some(to_ix)) = (row_index(rows, from), row_index(rows, to)) else {
            self.select_only(to);
            return;
        };
        self.selected = rows[from_ix.min(to_ix)..=from_ix.max(to_ix)]
            .iter()
            .map(|entry| entry.seq)
            .collect();
    }

    /// Moves the cursor one shown row, starting at the last row when there is none, and
    /// selects it, or the anchor through it when `extend`. Returns the cursor's row index.
    fn move_cursor(&mut self, rows: &[LogEntry], down: bool, extend: bool) -> Option<usize> {
        let last = rows.len().checked_sub(1)?;
        let ix = match self.cursor.and_then(|seq| row_index(rows, seq)) {
            Some(ix) if down => (ix + 1).min(last),
            Some(ix) => ix.saturating_sub(1),
            None => last,
        };
        let seq = rows[ix].seq;
        match self.anchor.or(self.cursor) {
            Some(anchor) if extend => {
                self.anchor = Some(anchor);
                self.cursor = Some(seq);
                self.select_range(rows, anchor, seq);
            }
            _ => self.select_only(seq),
        }
        Some(ix)
    }

    fn select_all(&mut self, rows: &[LogEntry]) {
        self.selected = rows.iter().map(|entry| entry.seq).collect();
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Forgets lines that left the ring. The ring holds consecutive `seq`s from its first line.
    fn prune(&mut self, first_retained: Option<u64>) {
        let Some(first) = first_retained else {
            self.clear();
            return;
        };
        self.selected.retain(|&seq| seq >= first);
        self.anchor = self.anchor.filter(|&seq| seq >= first);
        self.cursor = self.cursor.filter(|&seq| seq >= first);
    }
}

fn row_index(rows: &[LogEntry], seq: u64) -> Option<usize> {
    rows.binary_search_by_key(&seq, |entry| entry.seq).ok()
}

/// Shown rows at or after `paused_after`, the first `seq` following stopped before.
fn new_line_count(rows: &[LogEntry], paused_after: Option<u64>) -> usize {
    paused_after.map_or(0, |next| {
        rows.len() - rows.partition_point(|entry| entry.seq < next)
    })
}

/// Reusable app-wide logs pane. Hosts a variable-height list and mirrors
/// [`LogStore`] changes without tying logs to any specific feature pane.
///
/// The owner supplies the search as a [`LogFilter`] through [`Self::set_filter`], binds the
/// `logs` actions in [`LOGS_PANE_KEY_CONTEXT`] and handles [`LogsPaneEvent`].
pub struct LogsPane {
    logs: LogStore,
    filter: LogFilter,
    /// Retained line count at the last refresh.
    all_count: usize,
    /// Retained lines the filter shows, oldest first.
    rows: Arc<[LogEntry]>,
    selection: LogSelection,
    /// First `seq` that counts as new while not following.
    paused_after: Option<u64>,
    focus_handle: FocusHandle,
    list_state: ListState,
    column_widths: LogColumnWidths,
    last_width: Option<Pixels>,
    follow_tail: bool,
    resizing: Option<LogResizeState>,
}

impl EventEmitter<LogsPaneEvent> for LogsPane {}

impl LogsPane {
    pub fn new(logs: LogStore, _window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let rows: Arc<[LogEntry]> = Arc::from(logs.logs());
        let list_state = ListState::new(rows.len(), ListAlignment::Bottom, LOG_LIST_OVERDRAW);
        let entity = cx.entity();
        list_state.set_scroll_handler(move |event, _window, cx| {
            entity.update(cx, |pane, cx| {
                pane.follow_tail = should_follow_tail(event.is_scrolled);
                pane.update_follow();
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
                        pane.apply_rows(rows);
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
            filter: LogFilter::default(),
            all_count: rows.len(),
            rows,
            selection: LogSelection::default(),
            paused_after: None,
            focus_handle: cx.focus_handle().tab_stop(true),
            list_state,
            column_widths: LogColumnWidths::default(),
            last_width: None,
            follow_tail: true,
            resizing: None,
        }
    }

    /// Shows only the retained lines `filter` passes. A change clears the selection and
    /// replaces the list, which keeps following the newest line if it was.
    pub fn set_filter(&mut self, filter: LogFilter, cx: &mut Context<'_, Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.selection.clear();
        self.paused_after = None;
        self.rows = self.shown_rows(self.logs.logs()).into();
        self.replace_all_list_items(self.rows.len());
        self.update_follow();
        cx.notify();
    }

    #[must_use]
    pub const fn filter(&self) -> &LogFilter {
        &self.filter
    }

    #[must_use]
    pub const fn is_filtered(&self) -> bool {
        !self.filter.is_empty()
    }

    #[must_use]
    pub const fn store(&self) -> &LogStore {
        &self.logs
    }

    /// Retained line count at the last refresh.
    #[must_use]
    pub const fn retained_count(&self) -> usize {
        self.all_count
    }

    /// Lines the filter shows.
    #[must_use]
    pub fn shown_count(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn selected_count(&self) -> usize {
        self.selection.selected.len()
    }

    /// The log list's focus handle, a tab stop that holds the pane's key context.
    #[must_use]
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Copies every shown line, scrolled out of view or not, and confirms with a toast.
    pub fn copy_shown(&self, window: &mut Window, cx: &mut App) {
        copy_entries(&self.rows, false, window, cx);
    }

    /// Drops every retained line and refreshes now. The filter stays.
    pub fn clear_store(&mut self, cx: &mut Context<'_, Self>) {
        self.logs.clear();
        self.apply_rows(Vec::new());
        cx.notify();
    }

    /// Refresh from a copy of the ring: filters it and keeps the list in step by `seq`.
    fn apply_rows(&mut self, all: Vec<LogEntry>) {
        self.selection.prune(all.first().map(|entry| entry.seq));
        let rows = self.shown_rows(all);
        if !self.is_following() {
            self.pin_scroll();
        }
        self.set_rows(rows);
        self.update_follow();
    }

    fn shown_rows(&mut self, all: Vec<LogEntry>) -> Vec<LogEntry> {
        self.all_count = all.len();
        all.into_iter()
            .filter(|entry| self.filter.matches(entry))
            .collect()
    }

    fn is_following(&self) -> bool {
        self.follow_tail && self.selection.selected.is_empty()
    }

    /// Starts counting new lines when following stops and forgets the count when it resumes.
    fn update_follow(&mut self) {
        if self.is_following() {
            self.paused_after = None;
        } else if self.paused_after.is_none() {
            self.paused_after = Some(self.rows.last().map_or(0, |entry| entry.seq + 1));
        }
    }

    /// Holds the view in place while not following. A bottom-aligned list without a scroll
    /// anchor always shows its newest rows, so anchor it at the current top. Later splices
    /// shift that anchor by the rows they drop before it.
    fn pin_scroll(&mut self) {
        if self.list_state.logical_scroll_top().item_ix >= self.list_state.item_count() {
            self.list_state
                .scroll_by(-self.list_state.viewport_bounds().size.height);
            self.follow_tail = false;
        }
    }

    fn jump_to_latest(&mut self, cx: &mut Context<'_, Self>) {
        self.selection.clear();
        self.follow_tail = true;
        self.list_state.scroll_to(ListOffset {
            item_ix: self.rows.len(),
            offset_in_item: px(0.0),
        });
        self.update_follow();
        cx.notify();
    }

    fn selected_entries(&self) -> Vec<LogEntry> {
        self.rows
            .iter()
            .filter(|entry| self.selection.selected.contains(&entry.seq))
            .cloned()
            .collect()
    }

    fn copy_selected(&self, messages_only: bool, window: &mut Window, cx: &mut App) {
        copy_entries(&self.selected_entries(), messages_only, window, cx);
    }

    fn press_row(
        &mut self,
        seq: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let toggle = event.modifiers.secondary(cx);
        self.selection
            .click(&self.rows, seq, event.modifiers.shift, toggle);
        self.update_follow();
        cx.notify();
    }

    fn right_press_row(&mut self, seq: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.focus_handle.focus(window, cx);
        if !self.selection.selected.contains(&seq) {
            self.selection.select_only(seq);
            self.update_follow();
        }
        cx.notify();
    }

    fn move_cursor(&mut self, down: bool, extend: bool, cx: &mut Context<'_, Self>) {
        let Some(ix) = self.selection.move_cursor(&self.rows, down, extend) else {
            return;
        };
        // Revealing needs a real scroll anchor; see `pin_scroll`.
        self.pin_scroll();
        self.list_state.scroll_to_reveal_item(ix);
        self.update_follow();
        cx.notify();
    }

    fn select_previous_line(
        &mut self,
        _: &SelectPreviousLine,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.move_cursor(false, false, cx);
    }

    fn select_next_line(&mut self, _: &SelectNextLine, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.move_cursor(true, false, cx);
    }

    fn extend_selection_up(
        &mut self,
        _: &ExtendSelectionUp,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.move_cursor(false, true, cx);
    }

    fn extend_selection_down(
        &mut self,
        _: &ExtendSelectionDown,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.move_cursor(true, true, cx);
    }

    fn select_all_shown_lines(
        &mut self,
        _: &SelectAllShownLines,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.selection.select_all(&self.rows);
        self.update_follow();
        cx.notify();
    }

    fn copy_selected_lines(
        &mut self,
        _: &CopySelectedLines,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.selection.selected.is_empty() {
            cx.propagate();
            return;
        }
        self.copy_selected(false, window, cx);
    }

    fn clear_line_selection(
        &mut self,
        _: &ClearLineSelection,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.selection.selected.is_empty() {
            // Let an outer Escape handler run, such as restoring a dialog's focus.
            cx.propagate();
            return;
        }
        self.selection.clear();
        self.update_follow();
        cx.notify();
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

    fn render_body(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let row_count = self.rows.len();
        let content = if row_count == 0 {
            self.render_empty(cx).into_any_element()
        } else {
            list(
                self.list_state.clone(),
                cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)),
            )
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

        let new_lines = new_line_count(&self.rows, self.paused_after);
        if new_lines > 0 {
            body = body.child(
                div()
                    .absolute()
                    .left_0()
                    .right(LOG_SCROLLBAR_WIDTH)
                    .bottom(LOG_NEW_LINES_INSET)
                    .flex()
                    .justify_center()
                    .child(
                        app_button(
                            "logs-new-lines",
                            format!("{} new {}", format_count(new_lines), lines_word(new_lines)),
                        )
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx))),
                    ),
            );
        }

        body
    }

    fn render_empty(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let empty = Empty::new().size_full();
        if self.all_count == 0 {
            return empty
                .header(EmptyHeader::new().title(EmptyTitle::new().child("No log lines yet")));
        }
        empty
            .header(EmptyHeader::new().title(EmptyTitle::new().child("No lines match")))
            .content(
                EmptyContent::new().child(
                    app_button("logs-clear-search", "Clear search")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(LogsPaneEvent::ClearSearch))),
                ),
            )
    }

    fn render_row(&self, ix: usize, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let Some(entry) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let seq = entry.seq;
        let (level_label, level_color) = level_style(entry.level);
        let bg = if self.selection.selected.contains(&seq) {
            rgb(theme::SELECTED_ROW_SURFACE)
        } else if ix.is_multiple_of(2) {
            rgb(theme::SURFACE_ELEVATED)
        } else {
            rgb(theme::SURFACE_ELEVATED_ALT)
        };
        let is_cursor = self.selection.cursor == Some(seq) && self.focus_handle.is_focused(window);
        let widths = self.column_widths;
        let text = &self.filter.text;
        let pane = cx.weak_entity();
        let target = entry.target.clone();

        div()
            .id(("log-row", seq))
            .relative()
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
                highlighted_text(&entry.target, text),
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
                    .child(highlighted_text(&entry.message, text)),
            )
            .when(is_cursor, |row| {
                row.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(2.0))
                        .bg(rgb(theme::ACCENT_BLUE)),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.press_row(seq, event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _event, window, cx| {
                    this.right_press_row(seq, window, cx);
                }),
            )
            .context_menu(move |menu, _window, cx| row_menu(&pane, &target, menu, cx))
            .into_any_element()
    }
}

/// The row context menu. The right press already selected the row if it was outside the
/// selection, so "Copy N lines" counts what it copies.
fn row_menu(
    pane: &WeakEntity<LogsPane>,
    target: &Arc<str>,
    menu: PopupMenu,
    cx: &App,
) -> PopupMenu {
    let Some(this) = pane.upgrade() else {
        return menu;
    };
    let this = this.read(cx);
    let selected = this.selection.selected.len();
    let emit = |event: LogsPaneEvent| {
        let pane = pane.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = pane.update(cx, |_, cx| cx.emit(event.clone()));
        }
    };
    let messages_pane = pane.clone();
    menu.action_context(this.focus_handle.clone())
        .menu(
            format!("Copy {} {}", format_count(selected), lines_word(selected)),
            Box::new(CopySelectedLines),
        )
        .item(
            PopupMenuItem::new("Copy messages only").on_click(move |_, window, cx| {
                let _ = messages_pane.update(cx, |pane, cx| pane.copy_selected(true, window, cx));
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(format!("Show only {target}"))
                .on_click(emit(LogsPaneEvent::ShowOnlyTarget(target.clone()))),
        )
        .item(
            PopupMenuItem::new(format!("Hide {target}"))
                .on_click(emit(LogsPaneEvent::HideTarget(target.clone()))),
        )
        .separator()
        .menu("Select all shown", Box::new(SelectAllShownLines))
}

fn copy_entries(entries: &[LogEntry], messages_only: bool, window: &mut Window, cx: &mut App) {
    if entries.is_empty() {
        return;
    }
    let text = if messages_only {
        format_log_messages(entries)
    } else {
        format_log_lines(entries)
    };
    let n = entries.len();
    copy_to_clipboard_with_custom_toast(
        text,
        format!("Copied {} {}", format_count(n), lines_word(n)),
        window,
        cx,
    );
}

/// `text` with every case-insensitive occurrence of `needle` (already lowercased) highlighted.
fn highlighted_text(text: &Arc<str>, needle: &str) -> StyledText {
    let style = HighlightStyle {
        color: Some(rgb(theme::TEXT).into()),
        background_color: Some(rgb(theme::SELECTED_SURFACE).into()),
        ..HighlightStyle::default()
    };
    StyledText::new(SharedString::from(text)).with_highlights(
        match_ranges(text, needle)
            .into_iter()
            .map(|range| (range, style)),
    )
}

fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    let lower = text.to_lowercase();
    // shortcut: offsets come from the lowercased copy, so a line whose lowercase form changes
    // byte length goes unhighlighted; map offsets char by char if non-ASCII logs need it.
    if lower.len() != text.len() {
        return Vec::new();
    }
    lower
        .match_indices(needle)
        .map(|(start, found)| start..start + found.len())
        .filter(|range| text.is_char_boundary(range.start) && text.is_char_boundary(range.end))
        .collect()
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

fn render_nowrap_log_cell(text: impl IntoElement, width: Pixels, color: Rgba) -> impl IntoElement {
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
            .key_context(LOGS_PANE_KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_previous_line))
            .on_action(cx.listener(Self::select_next_line))
            .on_action(cx.listener(Self::extend_selection_up))
            .on_action(cx.listener(Self::extend_selection_down))
            .on_action(cx.listener(Self::select_all_shown_lines))
            .on_action(cx.listener(Self::copy_selected_lines))
            .on_action(cx.listener(Self::clear_line_selection))
            .on_action(cx.listener(|_, _: &FocusLogsSearch, _, cx| {
                cx.emit(LogsPaneEvent::FocusSearch);
            }))
            .relative()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(self.render_header(cx))
            .child(self.render_body(cx))
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

    fn entry(seq: u64, level: Level, target: &str, message: &str) -> LogEntry {
        LogEntry {
            seq,
            unix_ms: 0,
            level,
            target: target.into(),
            message: message.into(),
        }
    }

    fn rows(seqs: &[u64]) -> Vec<LogEntry> {
        seqs.iter()
            .map(|&seq| entry(seq, Level::INFO, "t", "m"))
            .collect()
    }

    #[test]
    fn default_log_capacity_is_5000() {
        assert_eq!(DEFAULT_LOG_CAPACITY, 5_000);
    }

    #[test]
    fn clear_keeps_sequence_counting() {
        let logs = LogStore::new(4);
        logs.push(entry(0, Level::INFO, "t", "before"));
        let rev = logs.rev();
        logs.clear();
        assert!(logs.logs().is_empty());
        assert!(logs.rev() > rev);
        logs.push(entry(0, Level::INFO, "t", "after"));
        assert_eq!(logs.logs()[0].seq, 1);
    }

    #[test]
    fn filter_matches_levels_targets_and_hides() {
        let shown = |filter: &LogFilter, targets: &[&str]| -> Vec<String> {
            targets
                .iter()
                .filter(|target| filter.matches(&entry(0, Level::INFO, target, "m")))
                .map(|target| (*target).to_owned())
                .collect()
        };
        let targets = [
            "wallet_ops::rpc_broker",
            "wallet_ops::rpc_broker::warmup",
            "wallet_ops::rpc_broker_metrics",
            "wallet_ops::poi",
            "arti_client",
            "arti_client::status",
        ];

        let only = LogFilter::new("", None, vec!["wallet_ops::rpc_broker".into()], vec![]);
        assert_eq!(
            shown(&only, &targets),
            ["wallet_ops::rpc_broker", "wallet_ops::rpc_broker::warmup"]
        );

        let hide = LogFilter::new(
            "",
            None,
            vec![],
            vec!["arti_client".into(), "wallet_ops::poi".into()],
        );
        assert_eq!(
            shown(&hide, &targets),
            [
                "wallet_ops::rpc_broker",
                "wallet_ops::rpc_broker::warmup",
                "wallet_ops::rpc_broker_metrics"
            ]
        );

        let warn = LogFilter::new("", Some(Level::WARN), vec![], vec![]);
        let levels: Vec<Level> = [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ]
        .into_iter()
        .filter(|&level| warn.matches(&entry(0, level, "t", "m")))
        .collect();
        assert_eq!(levels, [Level::ERROR, Level::WARN]);

        let text = LogFilter::new("  RPC ", None, vec![], vec![]);
        assert!(text.matches(&entry(0, Level::INFO, "t", "Rpc request failed")));
        assert!(text.matches(&entry(0, Level::INFO, "wallet_ops::rpc_broker", "m")));
        assert!(!text.matches(&entry(0, Level::INFO, "t", "m")));
    }

    #[test]
    fn copied_lines_use_rfc3339_utc_millis() {
        let mut line = entry(0, Level::WARN, "wallet_ops::rpc_broker", "slow");
        // 2026-11-05T14:03:22.123Z
        line.unix_ms = 1_793_887_402_123;
        let lines = [line.clone(), line];
        assert_eq!(
            format_log_lines(&lines),
            "2026-11-05T14:03:22.123Z WARN wallet_ops::rpc_broker: slow\n\
             2026-11-05T14:03:22.123Z WARN wallet_ops::rpc_broker: slow"
        );
        assert_eq!(format_log_messages(&lines), "slow\nslow");
        assert_eq!(format_count(4_812), "4,812");
        assert_eq!(format_count(1_000_000), "1,000,000");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn selection_ranges_toggles_and_prunes() {
        let rows = rows(&[10, 11, 12, 13, 14]);
        let mut selection = LogSelection::default();

        selection.click(&rows, 11, false, false);
        selection.click(&rows, 13, true, false);
        assert_eq!(selection.selected, BTreeSet::from([11, 12, 13]));

        selection.click(&rows, 12, false, true);
        assert_eq!(selection.selected, BTreeSet::from([11, 13]));

        selection.prune(Some(12));
        assert_eq!(selection.selected, BTreeSet::from([13]));
        selection.prune(Some(14));
        assert!(selection.selected.is_empty());
        assert_eq!((selection.anchor, selection.cursor), (None, None));

        assert_eq!(selection.move_cursor(&rows, false, false), Some(4));
        assert_eq!(selection.move_cursor(&rows, false, true), Some(3));
        assert_eq!(selection.selected, BTreeSet::from([13, 14]));
    }

    #[test]
    fn new_lines_count_shown_rows_after_pause() {
        let rows = rows(&[3, 5, 8, 9]);
        assert_eq!(new_line_count(&rows, None), 0);
        assert_eq!(new_line_count(&rows, Some(6)), 2);
        assert_eq!(new_line_count(&rows, Some(10)), 0);
    }

    #[gpui::test]
    fn filter_change_clears_selection_and_new_lines_keep_it(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let logs = LogStore::new(16);
        logs.push(entry(0, Level::ERROR, "t", "first error"));
        let (pane, cx) = cx.add_window_view(|window, cx| LogsPane::new(logs.clone(), window, cx));

        pane.update(cx, |pane, cx| {
            pane.selection.select_only(0);
            pane.set_filter(LogFilter::new("", Some(Level::ERROR), vec![], vec![]), cx);
            assert_eq!(pane.selected_count(), 0);

            logs.push(entry(0, Level::WARN, "t", "warning"));
            logs.push(entry(0, Level::ERROR, "t", "second error"));
            pane.apply_rows(logs.logs());
            let messages: Vec<&str> = pane.rows.iter().map(|row| &*row.message).collect();
            assert_eq!(messages, ["first error", "second error"]);
            assert_eq!(pane.list_state.item_count(), 2);
            assert_eq!((pane.retained_count(), pane.shown_count()), (3, 2));
        });
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
    fn compact_log_value_keeps_short_json_and_cuts_html_and_long_bodies() {
        let json =
            r#"HTTP error 429 with body: {"error":{"code":-32005,"message":"rate limited"}}"#;
        assert_eq!(compact_log_value(json), json);

        let html = format!(
            "HTTP error 502 with body: \n<!DOCTYPE html><html>{}</html>",
            "x".repeat(10)
        );
        assert_eq!(
            compact_log_value(&html),
            "HTTP error 502 with body: …[HTML, 38 bytes omitted]"
        );

        let long = format!("deserialization error: {}", "é".repeat(400));
        let compacted = compact_log_value(&long);
        assert!(compacted.ends_with(&format!("…[{} bytes omitted]", long.len() - 511)));
    }

    #[test]
    fn ui_log_layer_redacts_url_credentials_paths_queries_and_fragments() {
        let logs = LogStore::new(4);
        let layer = UiLogLayer::new(logs.clone());
        let subscriber = Registry::default().with(layer);
        // Byte MAX_LOG_FIELD_LEN falls inside `secret`, the debug field's leading quote included.
        let long_str = format!("{}https://user:secret@host/", "x".repeat(496));
        let long_debug = format!("{}https://user:secret@host/", "x".repeat(495));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                rpc = "https://user:pass@example.com/private/project-id?token=secret#fragment",
                proxy_url = "socks5h://proxy-user:proxy-pass@proxy.example:9050",
                long_str = long_str.as_str(),
                long_debug = ?long_debug,
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
        assert!(!entry.message.contains("sec"));
        assert!(!entry.message.contains("fragment"));
    }
}
