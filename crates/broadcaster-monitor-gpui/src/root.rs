use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use broadcaster_monitor::{EventRx, Shared};
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled, Window,
    canvas, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    IndexPath,
    input::{InputEvent, InputState},
    resizable::{ResizableState, h_resizable, resizable_panel},
    select::{SearchableVec, SelectEvent, SelectState},
    table::{DataTable, TableDelegate, TableEvent, TableState},
};
use ui::table::ColumnWidthSync;
use ui::theme;

use crate::fees_view::{
    BroadcasterPreferenceHooks, FeeAnchorLookup, FeesChainFilterItem, FeesDelegate, FeesFilter,
    FeesTokenFilterItem,
};
use crate::peers_view::{self, PeersDelegate};

/// Lower bound between UI re-renders when events are arriving.
const UI_REFRESH_THROTTLE: Duration = Duration::from_millis(100);

/// Periodic wakeup that updates relative timestamp cells at a bounded rate.
const UI_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

const MONITOR_PANE_GUTTER: Pixels = px(8.0);
const MONITOR_SPLIT_GUTTER: Pixels = px(6.0);
const MONITOR_SPLIT_COVER_WIDTH: Pixels = px(7.0);

pub struct BroadcasterMonitorPane {
    shared: Shared,
    top_split: Entity<ResizableState>,
    fees_table: Entity<TableState<FeesDelegate>>,
    peers_table: Entity<TableState<PeersDelegate>>,
    /// Last pane width measured in layout; used to avoid refreshing the table on every paint.
    last_pane_width: Option<Pixels>,
}

impl BroadcasterMonitorPane {
    pub fn set_chain_filter(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.fees_table.update(cx, |state, cx| {
            state
                .delegate_mut()
                .set_chain_filter(FeesFilter::One(chain_id));
            state.delegate_mut().sync_filter_selects(window, cx);
            cx.notify();
        });
    }

    pub fn set_preference_hooks(
        &mut self,
        hooks: Option<BroadcasterPreferenceHooks>,
        cx: &mut Context<'_, Self>,
    ) {
        self.fees_table.update(cx, |state, cx| {
            state.delegate_mut().set_preference_hooks(hooks);
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    pub fn refresh_preference_status(&self, cx: &mut Context<'_, Self>) {
        self.fees_table.update(cx, |_state, cx| {
            cx.notify();
        });
    }

    pub fn new(
        shared: Shared,
        mut event_rx: EventRx,
        chain_ids: &[u64],
        initial_chain_id: u64,
        default_fee_tokens: Vec<(u64, Address)>,
        fee_anchor_lookup: FeeAnchorLookup,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let top_split = cx.new(|_| ResizableState::default());
        let broadcaster_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("search broadcaster"));
        let chain_select_items: Vec<_> = std::iter::once(FeesChainFilterItem::all())
            .chain(chain_ids.iter().copied().map(FeesChainFilterItem::chain))
            .collect();
        let initial_chain_index = chain_ids
            .iter()
            .position(|chain_id| *chain_id == initial_chain_id)
            .map_or(0, |ix| ix + 1);
        let initial_chain_filter = if initial_chain_index == 0 {
            FeesFilter::All
        } else {
            FeesFilter::One(initial_chain_id)
        };
        let chain_select = cx.new(|cx| {
            SelectState::new(
                chain_select_items,
                Some(IndexPath::default().row(initial_chain_index)),
                window,
                cx,
            )
        });
        let token_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![FeesTokenFilterItem::all(false)]),
                Some(IndexPath::default().row(0)),
                window,
                cx,
            )
            .searchable(true)
        });
        let fees_table = cx.new(|cx| {
            let mut state = TableState::new(
                FeesDelegate::new(
                    broadcaster_input.clone(),
                    chain_select.clone(),
                    initial_chain_filter,
                    token_select.clone(),
                    default_fee_tokens,
                    fee_anchor_lookup,
                ),
                window,
                cx,
            );
            state.delegate_mut().sync_filter_selects(window, cx);
            state
        });
        let peers_table = cx.new(|cx| TableState::new(PeersDelegate::new(), window, cx));

        cx.subscribe(&broadcaster_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query: Arc<str> = Arc::from(input.read(cx).value().to_ascii_lowercase());
                this.fees_table.update(cx, |state, cx| {
                    state.delegate_mut().set_broadcaster_query(query);
                    cx.notify();
                });
            }
        })
        .detach();

        cx.subscribe_in(
            &chain_select,
            window,
            |this, _select, event: &SelectEvent<Vec<FeesChainFilterItem>>, window, cx| {
                let SelectEvent::Confirm(Some(filter)) = event else {
                    return;
                };
                this.fees_table.update(cx, |state, cx| {
                    state.delegate_mut().set_chain_filter(*filter);
                    state.delegate_mut().sync_filter_selects(window, cx);
                    cx.notify();
                });
                cx.defer_in(window, |_this, window, cx| window.blur(cx));
            },
        )
        .detach();
        cx.subscribe_in(
            &token_select,
            window,
            |this, _select, event: &SelectEvent<SearchableVec<FeesTokenFilterItem>>, window, cx| {
                let SelectEvent::Confirm(Some(filter)) = event else {
                    return;
                };
                this.fees_table.update(cx, |state, cx| {
                    state.delegate_mut().set_token_filter(*filter);
                    state.delegate_mut().sync_filter_selects(window, cx);
                    cx.notify();
                });
                cx.defer_in(window, |_this, window, cx| window.blur(cx));
            },
        )
        .detach();

        Self::subscribe_column_width_sync(cx, &peers_table);

        // The peers table only occupies the right side of `top_split`, so its
        // fill column must track that panel's width rather than the whole window.
        cx.observe(&top_split, |this, _, cx| {
            this.sync_peers_addr_width(None, cx);
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let mut last_rev: u64 = 0;
            loop {
                let tick = cx.background_executor().timer(UI_REFRESH_INTERVAL);
                tokio::select! {
                    changed = event_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = tick => {}
                }

                let notified = this.update_in(cx, |root, window, cx| {
                    let state = root.shared.read();
                    let current = state.rev();
                    let changed = current != last_rev;
                    let fees = changed.then(|| state.fee_rows());
                    let peers = changed.then(|| state.peer_rows());
                    last_rev = current;
                    drop(state);

                    root.fees_table.update(cx, |s, cx| {
                        if let Some(fees) = fees {
                            s.delegate_mut().set_rows(fees);
                        } else {
                            s.delegate_mut().refresh_anchor_values();
                        }
                        s.delegate_mut().sync_filter_selects(window, cx);
                        cx.notify();
                    });
                    if let Some(peers) = peers {
                        root.peers_table.update(cx, |s, cx| {
                            s.delegate_mut().set_rows(peers);
                            cx.notify();
                        });
                    }
                    cx.notify();
                    true
                });
                match notified {
                    Err(_) => break,
                    Ok(false) => {}
                    Ok(true) => {
                        cx.background_executor().timer(UI_REFRESH_THROTTLE).await;
                    }
                }
            }
        })
        .detach();

        Self {
            shared,
            top_split,
            fees_table,
            peers_table,
            last_pane_width: None,
        }
    }

    fn sync_peers_addr_width(&self, viewport_width: Option<Pixels>, cx: &mut Context<'_, Self>) {
        let top_split = self.top_split.read(cx);
        let Some(current_peers_width) = top_split.sizes().get(1).copied() else {
            return;
        };

        let peers_pane_width = if let Some(viewport_width) = viewport_width {
            let total_width = top_split
                .sizes()
                .iter()
                .copied()
                .fold(px(0.0), |sum, width| sum + width);
            if total_width == px(0.0) {
                current_peers_width
            } else {
                viewport_width * (current_peers_width / total_width)
            }
        } else {
            current_peers_width
        };

        self.peers_table.update(cx, |s, cx| {
            let target = peers_pane_width - MONITOR_SPLIT_GUTTER - s.delegate().addr_chrome();
            if s.delegate().addr_width() != target.max(px(120.0)) {
                s.delegate_mut().set_addr_width(target);
                s.refresh(cx);
            }
        });
    }

    /// Mirror user-dragged column widths back into the delegate so any later
    /// `TableState::refresh` keeps the latest runtime widths as source of truth.
    fn subscribe_column_width_sync<D>(cx: &mut Context<'_, Self>, table: &Entity<TableState<D>>)
    where
        D: TableDelegate + ColumnWidthSync,
    {
        cx.subscribe(table, |_this, state, event: &TableEvent, cx| {
            if let TableEvent::ColumnWidthsChanged(widths) = event {
                state.update(cx, |s, _| {
                    s.delegate_mut().apply_column_widths(widths);
                });
            }
        })
        .detach();
    }
}

impl Render for BroadcasterMonitorPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let entity = cx.entity();
        let peer_summary = self.shared.read().peer_summary();
        let split_boundary = self.top_split.read(cx).sizes().first().copied();

        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(theme::SURFACE_ELEVATED))
            .p(MONITOR_PANE_GUTTER)
            .child(
                div()
                    .relative()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(
                        h_resizable("broadcaster-monitor-top")
                            .with_state(&self.top_split)
                            .child(
                                resizable_panel().child(
                                    div()
                                        .size_full()
                                        .min_w(px(0.0))
                                        .min_h(px(0.0))
                                        .pr(MONITOR_SPLIT_GUTTER)
                                        .child(DataTable::new(&self.fees_table)),
                                ),
                            )
                            .child(
                                resizable_panel().child(
                                    div()
                                        .size_full()
                                        .min_w(px(0.0))
                                        .min_h(px(0.0))
                                        .pl(MONITOR_SPLIT_GUTTER)
                                        .child(peers_view::render_pane(
                                            &peer_summary,
                                            &self.peers_table,
                                        )),
                                ),
                            ),
                    )
                    .when_some(split_boundary, |this, split_boundary| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left(
                                    split_boundary - px(f32::from(MONITOR_SPLIT_COVER_WIDTH) / 2.0),
                                )
                                .h_full()
                                .w(MONITOR_SPLIT_COVER_WIDTH)
                                .bg(rgb(theme::SURFACE_ELEVATED)),
                        )
                    })
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                entity.update(cx, |this, cx| {
                                    let pane_width = bounds.size.width;
                                    if this.last_pane_width != Some(pane_width) {
                                        this.last_pane_width = Some(pane_width);
                                        this.sync_peers_addr_width(Some(pane_width), cx);
                                    }
                                });
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
    }
}
