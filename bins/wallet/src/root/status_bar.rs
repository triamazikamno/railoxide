use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, DragMoveEvent, Entity, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, deferred, div, img, prelude::FluentBuilder as _,
    px, relative, rgb,
};
use gpui_component::{
    Disableable as _, FocusTrapElement as _, Icon, IconName, Selectable as _, Sizable,
    button::{Button, ButtonVariants},
    input,
    popover::Popover,
    progress::Progress as UiProgress,
    separator::Separator,
    spinner::Spinner,
    status_bar::StatusBar,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button_base, app_muted_text};
use ui::icons;
use ui::logs::{ClearLineSelection, LogsPane, LogsPaneEvent, format_count};
use ui::network_status::{
    NetworkStatusTrigger, TorExitIpQueryState, network_status_pill, network_status_popover_content,
    network_status_scroll,
};
use ui::theme::{self, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};
use wallet_ops::ProverCacheBuildProgress;

use crate::assets::{RailgunActionIcon, RailgunSidebarIcon};

use super::actions::LOGS_PANEL_KEY_CONTEXT;
use super::logs_search::{self, line_count};
use super::{LOGS_DRAWER_MAX_HEIGHT, LOGS_DRAWER_MIN_HEIGHT, WalletRoot};

/// Fits the network chip's two text lines. A dialog at maximum height ends 16px above the window
/// bottom with its footer inside a 16px bottom padding, so a bar over 32px would cover its footer
/// buttons while the bar is lifted above dialogs.
const STATUS_BAR_HEIGHT: Pixels = px(32.0);
/// The sidebar and workspace row keeps at least this height when the logs panel grows.
const WORKSPACE_MIN_HEIGHT: Pixels = px(200.0);
const LOGS_PANEL_RESIZE_HANDLE_HEIGHT: Pixels = px(5.0);
/// Fits the search field's 32px input.
const LOGS_TITLE_BAR_HEIGHT: Pixels = px(40.0);
/// Narrower windows show a search's count as "N lines" without "of M".
const LOGS_TOTAL_COUNT_MIN_WIDTH: Pixels = px(960.0);

/// Drag payload and drag view of the logs panel's top-edge resize handle.
struct LogsPanelResize;

impl Render for LogsPanelResize {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<'_, Self>,
    ) -> impl IntoElement {
        div().size(px(1.0))
    }
}

impl WalletRoot {
    /// The logs panel and the status bar. While a dialog is open the block is lifted above
    /// the dialog layer in its own layout slot, and every element id inside stays the same.
    pub(super) fn render_bottom_block(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        let block = div()
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .when(self.logs_open, |this| {
                this.child(self.render_logs_panel(root, dialog_open, window, cx))
            })
            .child(self.render_status_bar(root, dialog_open));
        if dialog_open {
            deferred(div().flex_none().occlude().child(block))
                .with_priority(gpui_kit::base::POPUP_PRIORITY)
                .into_any_element()
        } else {
            block.into_any_element()
        }
    }

    fn render_status_bar(&self, root: &Entity<Self>, dialog_open: bool) -> impl IntoElement {
        let (center, chips) = self.render_wallet_status(root, dialog_open);
        let item = |element: AnyElement| div().flex_none().child(element);
        let bar = StatusBar::new()
            .h(STATUS_BAR_HEIGHT)
            .py_0()
            .bg(rgb(theme::SURFACE))
            .border_color(rgb(theme::BORDER))
            .text_size(APP_TEXT_SIZE)
            .line_height(relative(APP_TEXT_LINE_HEIGHT))
            .left(item(self.render_logs_toggle(root, dialog_open)))
            .left(item(self.render_gateway_status_icon(root, dialog_open)))
            .left(item(
                self.render_network_status_control(root).into_any_element(),
            ))
            .when_some(self.prover_cache_build_progress.clone(), |bar, progress| {
                bar.left(item(self.render_prover_cache_build_indicator(
                    root,
                    progress,
                    dialog_open,
                )))
            })
            .children(center)
            .map(|bar| {
                chips
                    .into_iter()
                    // The padding keeps count badges inside the region, which clips overflow.
                    .fold(bar, |bar, chip| bar.right(item(chip).py_1()))
            });
        div()
            .w_full()
            .debug_selector(|| "wallet-status-bar".into())
            .child(bar)
    }

    fn render_logs_toggle(&self, root: &Entity<Self>, dialog_open: bool) -> AnyElement {
        let root = root.clone();
        Button::new("wallet-status-bar-logs")
            .ghost()
            .small()
            .px_0p5()
            .accessibility_label("Logs")
            .tooltip("Logs")
            // A child icon keeps the shared size; `.icon()` would size it from the button.
            .child(Icon::new(RailgunSidebarIcon::Logs).size(theme::STATUS_BAR_ICON_SIZE))
            .selected(self.logs_open)
            .toggled(self.logs_open)
            .debug_selector(|| "wallet-status-bar-logs".into())
            // Above a dialog a press must not take focus from the dialog.
            .when(dialog_open, |this| {
                this.capture_any_mouse_down(|_, window, _| window.prevent_default())
            })
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    if root.logs_open {
                        root.close_logs_panel(window, cx);
                    } else {
                        root.logs_open = true;
                        cx.notify();
                    }
                });
            })
            .into_any_element()
    }

    pub(super) fn logs_pane_event(
        &mut self,
        _pane: &Entity<LogsPane>,
        event: &LogsPaneEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.logs_search.update(cx, |search, cx| match event {
            LogsPaneEvent::FocusSearch => search.focus(window, cx),
            LogsPaneEvent::ShowOnlyTarget(target) => {
                search.edit_chips(|chips| logs_search::show_only(chips, target), cx);
            }
            LogsPaneEvent::HideTarget(target) => {
                search.edit_chips(|chips| logs_search::hide(chips, target), cx);
            }
            LogsPaneEvent::ClearSearch => {
                search.clear(cx);
                // `clear` opens the suggestions for a focused field; this field may not be.
                search.dismiss_suggestions(cx);
            }
        });
    }

    fn close_logs_panel(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.logs_open = false;
        self.restore_logs_focus(window, cx);
        cx.notify();
    }

    /// Before a press in the panel above a dialog takes focus, remembers where focus is.
    fn remember_logs_focus(&mut self, window: &Window, cx: &App) {
        if !self.logs_panel_focus.contains_focused(window, cx) {
            self.logs_return_focus = window.focused(cx);
        }
    }

    /// Gives focus back to where it was before the panel took it, while the panel still holds
    /// it, and forgets it either way.
    fn restore_logs_focus(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(focus) = self.logs_return_focus.take()
            && self.logs_panel_focus.contains_focused(window, cx)
        {
            focus.focus(window, cx);
        }
    }

    /// Escape that nothing inside the panel took: the search field's suggestions and the
    /// selection come first. Clears the search text, keeping its chips, then gives focus back.
    fn logs_panel_escape(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let search = self.logs_search.read(cx);
        if search.text().is_empty() {
            self.restore_logs_focus(window, cx);
            return;
        }
        let input = search.input().clone();
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.logs_search.update(cx, |search, cx| {
            search.set_query("", cx);
            search.dismiss_suggestions(cx);
        });
    }

    /// Above a dialog, presses on the search field and the log list take focus, and the
    /// panel traps Tab; every other press keeps focus where it is.
    fn render_logs_panel(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        let viewport = window.viewport_size();
        let height = self
            .logs_panel_height
            .min(viewport.height - STATUS_BAR_HEIGHT - WORKSPACE_MIN_HEIGHT)
            .max(LOGS_DRAWER_MIN_HEIGHT);
        let resize_root = root.clone();
        let escape_root = root.clone();
        let clear_root = root.clone();
        let panel = div()
            .id("wallet-logs-panel")
            .debug_selector(|| "wallet-logs-panel".into())
            .key_context(LOGS_PANEL_KEY_CONTEXT)
            // Reached only when the search field and the log list had nothing to dismiss.
            .on_action(move |_: &input::Escape, window, cx| {
                escape_root.update(cx, |root, cx| root.logs_panel_escape(window, cx));
            })
            .on_action(move |_: &ClearLineSelection, window, cx| {
                clear_root.update(cx, |root, cx| root.logs_panel_escape(window, cx));
            })
            .relative()
            .flex_none()
            .w_full()
            .h(height)
            .child(self.render_logs_drawer(root, dialog_open, viewport.width, cx))
            .child(
                div()
                    .id("wallet-logs-panel-resize")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(LOGS_PANEL_RESIZE_HANDLE_HEIGHT)
                    .cursor_row_resize()
                    .occlude()
                    .when(dialog_open, |this| {
                        this.capture_any_mouse_down(|_, window, _| window.prevent_default())
                    })
                    .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                    .on_drag(LogsPanelResize, |_, _, _, cx| cx.new(|_| LogsPanelResize))
                    .on_drag_move(move |event: &DragMoveEvent<LogsPanelResize>, window, cx| {
                        let height = (window.viewport_size().height
                            - STATUS_BAR_HEIGHT
                            - event.event.position.y)
                            .max(LOGS_DRAWER_MIN_HEIGHT)
                            .min(LOGS_DRAWER_MAX_HEIGHT);
                        cx.stop_propagation();
                        resize_root.update(cx, |root, cx| {
                            if root.logs_panel_height != height {
                                root.logs_panel_height = height;
                                cx.notify();
                            }
                        });
                    }),
            );
        let panel = panel.track_focus(&self.logs_panel_focus);
        if dialog_open {
            // GPUI applies the dialog's trap only while focus is inside it. gpui-base keeps a
            // trap registered while its handle lives, so the trap gets a handle of its own
            // that no frame without a dialog tracks.
            div()
                .flex_none()
                .w_full()
                .child(panel)
                .focus_trap("wallet-logs-panel-trap", &self.logs_panel_trap)
                .into_any_element()
        } else {
            panel.into_any_element()
        }
    }

    /// The title bar, which reads: icon and "Logs", the search field, the counts, copy, clear,
    /// a divider and close, over the log list.
    fn render_logs_drawer(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
        viewport_width: Pixels,
        cx: &App,
    ) -> impl IntoElement {
        let pane = self.logs.read(cx);
        let (shown, retained, selected) = (
            pane.shown_count(),
            pane.retained_count(),
            pane.selected_count(),
        );
        let counts = if pane.is_filtered() && viewport_width >= LOGS_TOTAL_COUNT_MIN_WIDTH {
            format!("{} of {}", format_count(shown), line_count(retained))
        } else {
            line_count(shown)
        };
        // Records where focus is before a press in the field or the list takes it.
        let remember = |root: &Entity<Self>| {
            let root = root.clone();
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| root.remember_logs_focus(window, cx));
            }
        };
        let copy_pane = self.logs.clone();
        let clear_pane = self.logs.clone();
        let close_root = root.clone();
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .border_t_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .debug_selector(|| "wallet-logs-title-bar".into())
                    .h(LOGS_TITLE_BAR_HEIGHT)
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .bg(rgb(theme::SURFACE))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    // Above a dialog a press here must not take focus from the dialog. The
                    // search field occludes the title bar, so this doesn't reach it.
                    .when(dialog_open, |this| {
                        this.capture_any_mouse_down(|_, window, _| window.prevent_default())
                    })
                    .child(img(icons::logs_icon_path()).size(px(16.0)).flex_none())
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(theme::TEXT))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Logs"),
                    )
                    .child(
                        div()
                            .debug_selector(|| "wallet-logs-search".into())
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .occlude()
                            .when(dialog_open, |this| {
                                this.capture_any_mouse_down(remember(root))
                            })
                            .child(self.logs_search.clone()),
                    )
                    .child(app_muted_text(counts).flex_none().whitespace_nowrap())
                    .when(selected > 0, |this| {
                        this.child(
                            app_muted_text(format!("{} selected", format_count(selected)))
                                .flex_none()
                                .whitespace_nowrap(),
                        )
                    })
                    .child(
                        app_button_base("wallet-logs-copy")
                            .ghost()
                            .xsmall()
                            .accessibility_label("Copy shown lines")
                            .tooltip(format!("Copy {}", line_count(shown)))
                            .icon(IconName::Copy)
                            .disabled(shown == 0)
                            .on_click(move |_event, window, cx| {
                                copy_pane.update(cx, |pane, cx| pane.copy_shown(window, cx));
                            }),
                    )
                    .child(
                        app_button_base("wallet-logs-clear")
                            .ghost()
                            .xsmall()
                            .accessibility_label("Clear all lines")
                            .tooltip(format!("Clear all {}", line_count(retained)))
                            .icon(Icon::new(RailgunActionIcon::Trash2))
                            .disabled(retained == 0)
                            .on_click(move |_event, _window, cx| {
                                clear_pane.update(cx, LogsPane::clear_store);
                            }),
                    )
                    .child(Separator::vertical().h(px(18.0)).color(rgb(theme::BORDER)))
                    .child(
                        app_button_base("close-wallet-logs-drawer")
                            .ghost()
                            .xsmall()
                            .accessibility_label("Hide logs")
                            .tooltip("Hide logs")
                            .icon(IconName::Close)
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, |root, cx| root.close_logs_panel(window, cx));
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .when(dialog_open, |this| {
                        this.capture_any_mouse_down(remember(root))
                    })
                    .child(self.logs.clone()),
            )
    }

    fn render_network_status_control(&self, root: &Entity<Self>) -> impl IntoElement {
        let status = self.network_status_presentation();
        let activity = self.network_activity_presentation();
        let download_rate = self.tor_download_rate;
        let popover_root = root.clone();
        let content_root = root.clone();
        let popover_focus = self.network_status_popover_focus.clone();
        let network_status_error = super::network::native_network_status_error(
            self.network_status_error.as_ref(),
            self.tor_rotation_failure,
        );
        let tor_exit_ip_query = self.tor_exit_ip_query.clone();
        let tor_state_reset_confirming = self.tor_state_reset_confirming;
        let tor_rotation_in_progress = self.tor_rotation.is_some();

        let trigger = network_status_pill(
            "wallet-network-status-pill-trigger",
            false,
            &status,
            activity.as_ref(),
            download_rate,
        )
        .debug_selector(|| "wallet-network-status-pill".into())
        // A pointer press must not focus the pill, so closing restores focus to the prior field.
        .capture_any_mouse_down(|_, window, _| window.prevent_default());

        Popover::new("wallet-network-status-popover")
            .p_0()
            .anchor(Anchor::BottomLeft)
            .open(self.network_status_popover_open)
            .overlay_closable(false)
            .track_focus(&popover_focus)
            .on_open_change(move |open, _window, cx| {
                popover_root.update(cx, |root, cx| {
                    root.set_network_status_popover_open(*open, cx);
                });
            })
            .trigger(NetworkStatusTrigger::new(
                trigger,
                &self.network_status_focus,
                &status,
            ))
            .content(move |_state, window, _cx| {
                let root = content_root.clone();
                let dismiss_root = content_root.clone();
                let copy = match tor_exit_ip_query {
                    TorExitIpQueryState::Success(ip) => Some(
                        clipboard_with_toast("wallet-network-exit-ip-copy", ip.to_string())
                            .into_any_element(),
                    ),
                    _ => None,
                };
                network_status_scroll(
                    network_status_popover_content(
                        &status,
                        network_status_error.clone(),
                        tor_exit_ip_query.clone(),
                        tor_state_reset_confirming,
                        tor_rotation_in_progress,
                        activity.as_ref(),
                        download_rate,
                        move |intent, _window, cx| {
                            root.update(cx, |root, cx| root.network_status_intent(intent, cx));
                        },
                        copy,
                    ),
                    window,
                )
                // Consume the outside press so it cannot also dismiss a dialog underneath.
                .on_mouse_down_out(move |_, _window, cx| {
                    cx.stop_propagation();
                    dismiss_root.update(cx, |root, cx| {
                        root.set_network_status_popover_open(false, cx);
                    });
                })
                .focus_trap("wallet-network-status-popover-trap", &popover_focus)
            })
    }

    fn render_prover_cache_build_indicator(
        &self,
        root: &Entity<Self>,
        progress: ProverCacheBuildProgress,
        dialog_open: bool,
    ) -> AnyElement {
        let popover_root = root.clone();
        let trigger = Button::new("wallet-prover-cache-build-pill-trigger")
            .accessibility_label("Prover cache build status")
            .ghost()
            .xsmall()
            .tab_stop(false)
            .debug_selector(|| "wallet-prover-cache-build-pill".into())
            .child(
                Spinner::new()
                    .icon(IconName::LoaderCircle)
                    .color(rgb(theme::INFO).into())
                    .xsmall(),
            )
            .child(
                div()
                    .text_color(rgb(theme::INFO))
                    .child("Building prover cache"),
            );
        if dialog_open {
            // Inert above a dialog: a press must not take focus from the dialog either.
            return trigger
                .capture_any_mouse_down(|_, window, _| window.prevent_default())
                .into_any_element();
        }

        Popover::new("wallet-prover-cache-build-popover")
            .open(self.prover_cache_build_popover_open)
            .on_open_change(move |open, _window, cx| {
                popover_root.update(cx, |root, cx| {
                    root.set_prover_cache_build_popover_open(*open, cx);
                });
            })
            .trigger(trigger)
            .content(move |_state, _window, _cx| {
                Self::render_prover_cache_build_popover_content(&progress)
            })
            .into_any_element()
    }

    fn render_prover_cache_build_popover_content(progress: &ProverCacheBuildProgress) -> gpui::Div {
        let percent = progress.percent();
        let variant = progress
            .current_variant
            .as_deref()
            .unwrap_or("Preparing variants");
        let variant_kind = match progress.current_variant_is_poi {
            Some(true) => "POI",
            Some(false) => "Railgun",
            None => "Variant",
        };
        let count_text = if progress.total_variants == 0 {
            "Preparing variant list...".to_string()
        } else {
            format!(
                "{} of {} variants complete",
                progress.completed_variants, progress.total_variants
            )
        };

        div()
            .w(px(320.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Spinner::new()
                            .icon(IconName::LoaderCircle)
                            .color(rgb(theme::INFO).into())
                            .with_size(px(16.0)),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::INFO))
                            .child(progress.stage.label()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        UiProgress::new("prover-cache-build-progress")
                            .flex_1()
                            .h(px(7.0))
                            .value(f32::from(percent)),
                    )
                    .child(
                        div()
                            .w(px(42.0))
                            .text_color(rgb(theme::INFO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(format!("{percent}%"))),
                    ),
            )
            .child(
                div()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .line_height(px(18.0))
                    .child(SharedString::from(count_text)),
            )
            .child(
                div()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .p(px(10.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(variant_kind),
                    )
                    .child(
                        div()
                            .font_family(theme::APP_MONO_FONT_FAMILY)
                            .text_size(px(12.0))
                            .line_height(px(17.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(variant.to_string())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(SharedString::from(format!(
                        "Succeeded: {}",
                        progress.succeeded_variants
                    )))
                    .child(SharedString::from(format!(
                        "Failed: {}",
                        progress.failed_variants
                    ))),
            )
    }
}
