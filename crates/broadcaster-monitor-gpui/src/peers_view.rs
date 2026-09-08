use std::sync::Arc;

use gpui::{
    App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb,
};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    popover::Popover,
    table::{Column, DataTable, TableDelegate, TableState},
    tooltip::Tooltip,
};

use broadcaster_monitor::{PeerRow, PeerSummary};
use ui::clipboard::clipboard_with_toast;
use ui::table::ColumnWidthSync;
use ui::theme::{self, APP_MONO_FONT_FAMILY};

/// `TableDelegate` backing the peers pane. The peer summary (connected /
/// known / dialing counts + capability tallies) is rendered outside the
/// table by [`render_pane`] — the delegate only owns the per-peer rows.
pub(crate) struct PeersDelegate {
    rows: Arc<[PeerRow]>,
    columns: [Column; 5],
}

impl PeersDelegate {
    pub(crate) fn new() -> Self {
        Self {
            rows: Arc::from(Vec::<PeerRow>::new()),
            columns: [
                Column::new("peer_id", "peer id")
                    .width(px(120.0))
                    .movable(false),
                Column::new("state", "state").width(px(80.0)).movable(false),
                Column::new("caps", "caps").width(px(140.0)).movable(false),
                Column::new("fails", "fails").width(px(60.0)).movable(false),
                Column::new("addr", "addr").width(px(320.0)).movable(false),
            ],
        }
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<PeerRow>) {
        let mut sorted = rows;
        sorted.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        self.rows = Arc::from(sorted);
    }

    /// Update the addr column's width. Clamped so the trailing address stays
    /// visible even when the peers pane is very narrow.
    pub(crate) fn set_addr_width(&mut self, width: Pixels) {
        const MIN_ADDR_WIDTH: Pixels = px(120.0);
        self.columns[4].width = width.max(MIN_ADDR_WIDTH);
    }

    /// Returns the addr column's current width.
    pub(crate) const fn addr_width(&self) -> Pixels {
        self.columns[4].width
    }

    /// Width consumed by every non-addr column plus table chrome.
    pub(crate) fn addr_chrome(&self) -> Pixels {
        self.columns[..4]
            .iter()
            .fold(px(40.0), |sum, col| sum + col.width)
    }
}

impl ColumnWidthSync for PeersDelegate {
    fn columns_mut(&mut self) -> &mut [Column] {
        &mut self.columns
    }
}

impl TableDelegate for PeersDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => {
                let peer_id = row.peer_id.to_string();
                let group = SharedString::from(format!("peer-id-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("peer-id-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_color(rgb(theme::PURPLE))
                    .child(SharedString::from(short(row.peer_id.as_ref(), 4)))
                    .child(
                        div()
                            .id(SharedString::from(format!("peer-id-copy-action-{row_ix}")))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| Tooltip::new("Copy peer ID").build(window, cx))
                            .child(clipboard_with_toast(
                                SharedString::from(format!("peer-id-clipboard-{row_ix}")),
                                peer_id,
                            )),
                    )
                    .into_any_element()
            }
            1 => {
                let (label, color) = if row.connected {
                    ("connected", rgb(theme::SUCCESS))
                } else if row.dialing {
                    ("dialing", rgb(theme::WARNING))
                } else {
                    ("known", rgb(theme::TEXT_MUTED))
                };
                div()
                    .text_color(color)
                    .child(SharedString::from(label))
                    .into_any_element()
            }
            2 => div()
                .text_color(rgb(theme::INFO))
                .child(SharedString::from(capabilities(row)))
                .into_any_element(),
            3 => div()
                .text_color(if row.dial_failures == 0 {
                    rgb(theme::TEXT_MUTED)
                } else {
                    rgb(theme::DANGER)
                })
                .child(SharedString::from(row.dial_failures.to_string()))
                .into_any_element(),
            _ => {
                let addrs = ordered_addrs(&row.addrs);
                let addr = preferred_addr(&row.addrs).map_or_else(
                    || SharedString::new_static("-"),
                    |s| SharedString::from(s.to_owned()),
                );
                let trigger_id = SharedString::from(format!("peer-address-cell-{row_ix}"));
                let popover_id = SharedString::from(format!("peer-address-popover-{row_ix}"));
                Popover::new(popover_id)
                    .trigger(
                        Button::new(trigger_id)
                            .text()
                            .xsmall()
                            .w_full()
                            .justify_start()
                            .child(
                                div()
                                    .w_full()
                                    .text_left()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(addr),
                            ),
                    )
                    .content(move |_state, _window, _cx| {
                        div().flex().flex_col().gap_1().min_w(px(320.0)).children(
                            addrs
                                .clone()
                                .into_iter()
                                .enumerate()
                                .map(move |(ix, addr)| {
                                    let label = SharedString::from(addr.as_ref().to_owned());
                                    let group = SharedString::from(format!(
                                        "peer-address-option-group-{row_ix}-{ix}"
                                    ));
                                    div()
                                        .group(group.clone())
                                        .id(SharedString::from(format!(
                                            "peer-address-option-{row_ix}-{ix}"
                                        )))
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_2()
                                        .py_1()
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .child(div().min_w_0().child(label.clone()))
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "peer-address-copy-action-{row_ix}-{ix}"
                                                )))
                                                .group(group.clone())
                                                .flex_none()
                                                .opacity(0.0)
                                                .group_hover(group, |this| this.opacity(1.0))
                                                .hover(|this| this.opacity(1.0))
                                                .tooltip(|window, cx| {
                                                    Tooltip::new("Copy peer address")
                                                        .build(window, cx)
                                                })
                                                .child(clipboard_with_toast(
                                                    SharedString::from(format!(
                                                        "peer-address-clipboard-{row_ix}-{ix}"
                                                    )),
                                                    label,
                                                )),
                                        )
                                }),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

/// Compose the peers pane: lightweight summary metadata above the peer table.
pub(crate) fn render_pane(
    summary: &PeerSummary,
    state: &Entity<TableState<PeersDelegate>>,
) -> impl IntoElement {
    let subtitle = format!(
        "{} connected · {} known · {} dialing · LP:{} PX:{}",
        summary.connected,
        summary.known,
        summary.dialing,
        summary.lightpush_capable,
        summary.peer_exchange_capable
    );

    div()
        .size_full()
        .flex()
        .flex_col()
        .gap_1()
        .min_h_0()
        .min_w_0()
        .child(
            div()
                .flex_none()
                .px_2()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(subtitle))
                .child(
                    div()
                        .mt(px(2.0))
                        .text_color(if summary.network_degraded {
                            rgb(theme::WARNING)
                        } else {
                            rgb(theme::SUCCESS)
                        })
                        .child(SharedString::from(summary.network_label.to_string())),
                ),
        )
        .child(div().flex_1().min_h_0().child(DataTable::new(state)))
}

fn capabilities(row: &PeerRow) -> String {
    let mut caps = Vec::new();
    if row.supports_filter {
        caps.push("filter");
    }
    if row.supports_lightpush_v3 {
        caps.push("lp3");
    }
    if row.supports_peer_exchange {
        caps.push("px");
    }
    if caps.is_empty() {
        "-".to_string()
    } else {
        caps.join(",")
    }
}

fn preferred_addr(addrs: &[Arc<str>]) -> Option<&str> {
    addrs
        .iter()
        .find(|addr| is_dns_addr(addr.as_ref()))
        .or_else(|| addrs.first())
        .map(Arc::as_ref)
}

fn ordered_addrs(addrs: &[Arc<str>]) -> Vec<Arc<str>> {
    let (mut dns, other): (Vec<_>, Vec<_>) = addrs
        .iter()
        .cloned()
        .partition(|addr| is_dns_addr(addr.as_ref()));
    dns.extend(other);
    dns
}

fn is_dns_addr(addr: &str) -> bool {
    addr.starts_with("/dns")
}

fn short(input: &str, chunk_size: usize) -> String {
    if input.len() <= chunk_size * 2 {
        return input.to_string();
    }
    let head = &input[..chunk_size];
    let tail = &input[input.len() - chunk_size..];
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use broadcaster_monitor::PeerRow;

    fn addrs(values: &[&str]) -> Vec<Arc<str>> {
        values.iter().map(|value| Arc::from(*value)).collect()
    }

    fn peer_row(peer_id: &str, connected: bool, dialing: bool) -> PeerRow {
        PeerRow {
            peer_id: Arc::from(peer_id),
            addrs: Vec::new(),
            connected,
            dialing,
            supports_lightpush_v3: false,
            supports_peer_exchange: false,
            supports_filter: false,
            dial_failures: 0,
        }
    }

    #[test]
    fn preferred_addr_uses_dns_when_present() {
        let addrs = addrs(&[
            "/ip4/127.0.0.1/tcp/30303/p2p/peer",
            "/dns4/example.com/tcp/30303/p2p/peer",
        ]);
        assert_eq!(
            preferred_addr(&addrs),
            Some("/dns4/example.com/tcp/30303/p2p/peer")
        );
    }

    #[test]
    fn preferred_addr_falls_back_to_first_addr() {
        let addrs = addrs(&[
            "/ip4/127.0.0.1/tcp/30303/p2p/peer",
            "/ip4/10.0.0.5/tcp/30303/p2p/peer",
        ]);
        assert_eq!(
            preferred_addr(&addrs),
            Some("/ip4/127.0.0.1/tcp/30303/p2p/peer")
        );
    }

    #[test]
    fn ordered_addrs_moves_dns_entries_first() {
        let addrs = addrs(&[
            "/ip4/127.0.0.1/tcp/30303/p2p/peer",
            "/dns4/example.com/tcp/30303/p2p/peer",
            "/ip4/10.0.0.5/tcp/30303/p2p/peer",
            "/dnsaddr/bootstrap.example/p2p/peer",
        ]);
        let ordered = ordered_addrs(&addrs)
            .into_iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            vec![
                "/dns4/example.com/tcp/30303/p2p/peer",
                "/dnsaddr/bootstrap.example/p2p/peer",
                "/ip4/127.0.0.1/tcp/30303/p2p/peer",
                "/ip4/10.0.0.5/tcp/30303/p2p/peer",
            ]
        );
    }

    #[test]
    fn set_rows_sorts_only_by_peer_id() {
        let mut delegate = PeersDelegate::new();
        delegate.set_rows(vec![
            peer_row("peer-c", true, false),
            peer_row("peer-a", false, true),
            peer_row("peer-b", false, false),
        ]);

        let ordered = delegate
            .rows
            .iter()
            .map(|row| row.peer_id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ordered, vec!["peer-a", "peer-b", "peer-c"]);
    }
}
