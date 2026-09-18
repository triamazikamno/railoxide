use super::observations::CheckAttempt;
use super::{
    AccountObservations, Context, Disableable, Entity, ExecutorAsset, ExecutorOperationId,
    ExecutorRecord, IntoElement, ParentElement, Render, SharedString, Sizable, StealthAccountsView,
    Styled, WalletRoot, Window, app_button, app_input, app_muted_text, app_segment_button,
    app_strong_text, asset_label, div,
};
use gpui::{
    App, InteractiveElement as _, RenderOnce, StatefulInteractiveElement as _,
    prelude::FluentBuilder as _, rems,
};
use gpui_component::button::ButtonVariants as _;
use gpui_component::{
    ActiveTheme as _, ChildElement, Icon, IconName, InteractiveElementExt as _, Size,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    pagination::Pagination,
    scroll::{ScrollableElement as _, Scrollbar, ScrollbarMode},
    table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow},
    tag::Tag,
};
use wallet_ops::{ExecutorAccountOutcome, ExecutorAccountStatus, vault::ExecutorRecordOrigin};

const ACCOUNTS_PER_PAGE: usize = 25;

#[derive(Default)]
pub(super) struct AccountPages {
    accounts: Vec<ExecutorOperationId>,
    page: usize,
    scroll: gpui::ScrollHandle,
    horizontal_scroll: gpui::ScrollHandle,
}

impl AccountPages {
    fn count(&self) -> usize {
        self.accounts.len().div_ceil(ACCOUNTS_PER_PAGE).max(1)
    }

    fn range(&self) -> std::ops::Range<usize> {
        let start = self.page * ACCOUNTS_PER_PAGE;
        start..(start + ACCOUNTS_PER_PAGE).min(self.accounts.len())
    }

    fn update(&mut self, accounts: Vec<ExecutorOperationId>) {
        if self.accounts == accounts {
            return;
        }
        self.accounts = accounts;
        self.page = self.page.min(self.count() - 1);
        self.scroll.set_offset(gpui::Point::default());
    }

    fn change(&mut self, page: usize) {
        let page = page.min(self.count() - 1);
        if self.page != page {
            self.page = page;
            self.scroll.set_offset(gpui::Point::default());
        }
    }

    fn first(&mut self) {
        self.change(0);
    }

    fn reveal(&mut self, operation: ExecutorOperationId) {
        if let Some(index) = self.accounts.iter().position(|id| *id == operation) {
            self.change(index / ACCOUNTS_PER_PAGE);
            self.scroll.scroll_to_top_of_item(index % ACCOUNTS_PER_PAGE);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountFilter {
    All,
    Attention,
    Holding,
    Used,
}

#[derive(IntoElement)]
pub(super) struct StealthSummary {
    pub view: Entity<StealthAccountsView>,
    pub root: Entity<WalletRoot>,
}

impl RenderOnce for StealthSummary {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let view = self.view.read(cx);
        let attention = view
            .records
            .iter()
            .filter(|record| view.needs_attention(record))
            .count();
        let holding = view
            .observations
            .values()
            .filter(|observations| observations.holding())
            .count();
        let checked = view.observations.values().any(|observations| {
            observations
                .assets
                .values()
                .any(|balance| balance.attempt != CheckAttempt::NotChecked)
        });
        let summary = if view.records_error.is_some() {
            "Saved accounts could not be loaded".into()
        } else {
            format!(
                "{} accounts · {}{}",
                view.records.len(),
                if attention == 0 {
                    "No issues detected".into()
                } else {
                    format!("{attention} need attention")
                },
                if checked {
                    format!(" · {holding} holding a checked balance")
                } else {
                    String::new()
                }
            )
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(app_strong_text("Stealth accounts"))
            .child(
                account_caption(
                    "Public one-time accounts your private operations execute through.",
                )
                .whitespace_normal(),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .bg(cx.theme().background)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(account_caption(summary).whitespace_normal()),
                    )
                    .child(
                        app_button("stealth-open", "Show")
                            .outline()
                            .small()
                            .on_click(move |_, window, cx| {
                                self.root
                                    .update(cx, |root, cx| root.open_stealth_accounts(window, cx));
                            }),
                    ),
            )
    }
}

impl StealthAccountsView {
    pub(super) fn status(&self, record: &ExecutorRecord) -> ExecutorAccountStatus {
        self.owner.account_status(record).unwrap_or_default()
    }

    pub(super) fn holding(&self, operation: ExecutorOperationId) -> bool {
        self.observations
            .get(&operation)
            .is_some_and(AccountObservations::holding)
    }

    pub(super) fn needs_attention(&self, record: &ExecutorRecord) -> bool {
        self.status(record).needs_attention() || self.holding(record.operation())
    }

    pub(super) fn assets_for(&self, record: &ExecutorRecord) -> Vec<ExecutorAsset> {
        let mut assets = std::collections::BTreeSet::from([ExecutorAsset::Native]);
        assets.extend(record.assets());
        if let Some(observations) = self.observations.get(&record.operation()) {
            assets.extend(observations.assets.keys());
        }
        assets.into_iter().collect()
    }

    pub(super) fn asset_name(&self, asset: ExecutorAsset, cx: &App) -> String {
        match asset {
            ExecutorAsset::Native => match self.session.chain_id {
                56 => "BNB",
                137 => "POL",
                _ => "ETH",
            }
            .into(),
            ExecutorAsset::Erc20(token) => self
                .root
                .upgrade()
                .and_then(|root| {
                    root.read(cx)
                        .effective_token_registry
                        .get(self.session.chain_id, &token)
                        .map(|info| info.symbol.clone())
                })
                .unwrap_or_else(|| "Token".into()),
            ExecutorAsset::Erc721 { token_id, .. } => format!("NFT #{token_id}"),
        }
    }

    pub(super) fn refresh_visible(&mut self, cx: &App) {
        let search = self.search.read(cx).value().trim().to_lowercase();
        if self.search_query != search {
            self.pages.first();
            self.search_query.clone_from(&search);
        }
        let visible = visible_accounts(
            &self.records,
            self.filter,
            self.show_hidden,
            &search,
            |record| {
                let status = self.status(record);
                let holding = self.holding(record.operation());
                AccountVisibility {
                    attention: status.needs_attention() || holding,
                    unresolved: status.unresolved(),
                    holding,
                }
            },
            |record| {
                self.assets_for(record)
                    .into_iter()
                    .map(|asset| format!("{} {}", self.asset_name(asset, cx), asset_label(asset)))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
        );
        self.pages.update(visible);
    }

    pub(super) fn reveal_account(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.show_hidden = true;
        self.filter = AccountFilter::All;
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.expanded = Some(operation);
        self.refresh_visible(cx);
        self.pages.reveal(operation);
        self.recover_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn balance_value_lines(
        &self,
        operation: ExecutorOperationId,
        asset: ExecutorAsset,
        cx: &App,
    ) -> (String, Option<String>) {
        let Some(balance) = self
            .observations
            .get(&operation)
            .and_then(|observations| observations.assets.get(&asset))
        else {
            return ("Not checked".into(), None);
        };
        let latest = match balance.attempt {
            CheckAttempt::NotChecked => "Not checked",
            CheckAttempt::Checking => "Checking…",
            CheckAttempt::Available => "",
            CheckAttempt::Unavailable => "Unavailable",
            CheckAttempt::Stopped => "Check stopped",
        };
        match balance.value {
            Some(value) => (
                self.recovery_amount_label(asset, value.amount, cx),
                Some(format!(
                    "{} · #{}{}",
                    observation_age(value.checked_at),
                    value.block.number,
                    if latest.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " · {latest}{}",
                            balance
                                .attempted_at
                                .map_or_else(String::new, |at| format!(" {}", observation_age(at)))
                        )
                    }
                )),
            ),
            None => (latest.into(), balance.attempted_at.map(observation_age)),
        }
    }

    fn render_row(
        &self,
        operation: ExecutorOperationId,
        cx: &Context<'_, Self>,
    ) -> Option<TableBody> {
        let record = self
            .records
            .iter()
            .find(|record| record.operation() == operation)?;
        let status = self.status(record);
        let assets = self.assets_for(record);
        let expanded = self.expanded == Some(operation);
        let dim = matches!(
            status.outcome(),
            ExecutorAccountOutcome::Executed | ExecutorAccountOutcome::RecoveryConfirmed
        ) && status.rechecked().is_some()
            && !status.unresolved()
            && self
                .observations
                .get(&operation)
                .is_some_and(|observations| {
                    observations.checked_assets_zero()
                        && observations
                            .inspection
                            .as_ref()
                            .is_some_and(|inspection| !inspection.has_incomplete_reads())
                });
        let account = div()
            .w_full()
            .flex()
            .flex_col()
            .items_start()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(ui::controls::app_text(format!("#{}", record.index())))
                    .when(
                        record.origin() == ExecutorRecordOrigin::Discovered,
                        |cell| cell.child(account_caption("Restored")),
                    )
                    .when(record.is_hidden(), |cell| {
                        cell.child(account_caption("Hidden"))
                    }),
            )
            .children(record.address().map(|address| {
                Self::address(
                    format!("stealth-row-address-{}", operation.opaque_id()).into(),
                    address,
                    cx,
                )
            }));
        let purpose = div()
            .w_full()
            .min_w_0()
            .children(record.purpose_summary().map(|purpose| {
                let intent = purpose
                    .split_once(" → ")
                    .map_or(purpose, |(intent, _)| intent);
                ui::controls::app_text(intent.to_owned()).whitespace_normal()
            }))
            .children(
                record
                    .created_at()
                    .or_else(|| record.restored_at())
                    .map(|at| {
                        account_caption(if record.created_at().is_some() {
                            timestamp_label(at)
                        } else {
                            format!("Restored {}", timestamp_label(at))
                        })
                    }),
            );
        let status_cell = if has_local_history(record) {
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || {
                            format!("stealth-outcome-{}", operation.opaque_id())
                        })
                        .child(outcome_tag(status.outcome())),
                )
        } else {
            let check = record.use_check();
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_start()
                .child(
                    Tag::secondary()
                        .outline()
                        .small()
                        .rounded_full()
                        .line_height(gpui::relative(ui::theme::APP_TEXT_LINE_HEIGHT))
                        .child(use_label(record)),
                )
                .children(check.observation().map(|observation| {
                    account_caption(format!(
                        "Checked #{} · {}",
                        observation.block().number,
                        timestamp_label(observation.checked_at())
                    ))
                    .whitespace_normal()
                }))
                .when(
                    check.is_unavailable() && check.observation().is_some(),
                    |cell| {
                        cell.child(
                            account_caption("Recheck unavailable").text_color(cx.theme().warning),
                        )
                    },
                )
        };
        let completed: Vec<_> = assets
            .iter()
            .filter_map(|asset| {
                self.observations
                    .get(&operation)
                    .and_then(|observations| observations.assets.get(asset))
                    .and_then(|balance| balance.value)
                    .map(|value| (*asset, value))
            })
            .collect();
        let checked = !completed.is_empty();
        let summary_assets: Vec<_> = completed
            .into_iter()
            .filter(|(_, value)| !value.amount.is_zero())
            .collect();
        let balances = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .when(!checked, |cell| cell.child(account_caption("Not checked")))
            .when(checked && summary_assets.is_empty(), |cell| {
                cell.child(account_caption("No positive balances"))
            })
            .when(checked, |cell| {
                cell.children(summary_assets.into_iter().map(|(asset, value)| {
                    div()
                        .child(app_strong_text(format!(
                            "{} {}",
                            self.recovery_amount_label(asset, value.amount, cx),
                            self.asset_name(asset, cx),
                        )))
                        .children(Self::asset_address(
                            format!(
                                "stealth-row-asset-{}-{}",
                                operation.opaque_id(),
                                asset_label(asset)
                            )
                            .into(),
                            asset,
                            cx,
                        ))
                        .child(
                            account_caption(format!(
                                "{} · #{}",
                                observation_age(value.checked_at),
                                value.block.number,
                            ))
                            .whitespace_normal(),
                        )
                }))
            });
        let menu_view = cx.entity();
        let actions = div()
            .id(SharedString::from(format!(
                "account-actions-{}",
                operation.opaque_id()
            )))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .justify_end()
            .when(expanded, |control| {
                control
                    .track_focus(&self.recover_focus)
                    .tab_group()
                    .tab_index(0)
                    .tab_stop(false)
            })
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if this.recover_focus.is_focused(window)
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        // The shared Button owns its focus handle. Enter its tab stop
                        // before dispatching the menu's normal keyboard activation.
                        window.focus_next(cx);
                        window.dispatch_action(
                            Box::new(gpui_kit::base::actions::Confirm { secondary: false }),
                            cx,
                        );
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                app_button(
                    SharedString::from(format!("account-menu-{}", operation.opaque_id())),
                    "⋯",
                )
                .ghost()
                .small()
                .accessibility_label(format!("Actions for account #{}", record.index()))
                .tooltip("Account actions")
                .debug_selector(move || format!("stealth-row-menu-{}", operation.opaque_id()))
                .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, cx| {
                    Self::account_menu(&menu_view, operation, menu, cx)
                }),
            )
            .child(
                ui::controls::app_button_base(SharedString::from(format!(
                    "account-expand-{}",
                    operation.opaque_id()
                )))
                .ghost()
                .small()
                .icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .accessibility_label(format!(
                    "{} account #{}",
                    if expanded { "Collapse" } else { "Expand" },
                    record.index()
                ))
                .tooltip(if expanded {
                    "Collapse account"
                } else {
                    "Expand account"
                })
                .debug_selector(move || format!("stealth-expand-{}", operation.opaque_id()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_expanded(operation, cx);
                })),
            );
        Some(account_rows(
            AccountMenuRow {
                row: TableRow::new()
                    .when(expanded, |row| row.bg(cx.theme().muted))
                    .when(dim, |row| row.opacity(0.6))
                    .child(TableCell::new().w(rems(8.)).flex_none().child(account))
                    .child(TableCell::new().flex_1().min_w(rems(11.)).child(purpose))
                    .child(TableCell::new().w(rems(11.)).flex_none().child(status_cell))
                    .child(TableCell::new().flex_1().min_w(rems(8.)).child(balances))
                    .child(
                        TableCell::new()
                            .w(rems(5.))
                            .min_w_0()
                            .flex_none()
                            .child(actions),
                    ),
                operation,
                view: cx.entity(),
            },
            expanded.then(|| self.render_inspector(record, cx)),
            cx,
        ))
    }

    fn toggle_expanded(&mut self, operation: ExecutorOperationId, cx: &mut Context<'_, Self>) {
        self.expanded = if self.expanded == Some(operation) {
            None
        } else {
            Some(operation)
        };
        self.refresh_visible(cx);
        cx.notify();
    }

    fn return_to_public(&self, cx: &mut App) {
        let _ = self.root.update(cx, |root, cx| {
            if let Some(panel) = &mut root.stealth_accounts {
                panel.open = false;
            }
            root.focus_public_account_search_on_render = true;
            cx.notify();
        });
    }

    fn account_menu(
        view: &Entity<Self>,
        operation: ExecutorOperationId,
        menu: PopupMenu,
        cx: &App,
    ) -> PopupMenu {
        let this = view.read(cx);
        let Some(record) = this
            .records
            .iter()
            .find(|record| record.operation() == operation)
        else {
            return menu;
        };
        let disabled = this.job.is_some() || record.address().is_none();
        let recovery_disabled = this.recovery_disabled_reason(record, cx).is_some();
        let holding = this.holding(operation);
        let pending_recovery = record.recovery_transactions().iter().any(|transaction| {
            record.recovery_transaction_status(transaction.hash())
                == Some(wallet_ops::vault::ExecutorPayloadStatus::Uncertain)
        });
        let public_label = if record.public_account_uuid().is_some() {
            "Open in Public"
        } else {
            "Add to Public"
        };
        let public_view = view.clone();
        let recover_view = view.clone();
        let hide_view = view.clone();
        let hide_disabled = this.job.is_some();
        let hide_label = if record.is_hidden() {
            "Unhide account"
        } else {
            "Hide account"
        };
        menu.item(
            PopupMenuItem::new(public_label)
                .disabled(disabled)
                .on_click(move |_, window, cx| {
                    public_view.update(cx, |view, cx| {
                        view.open_public_account(operation, window, cx);
                    });
                }),
        )
        .when(holding || pending_recovery, |menu| {
            menu.item(
                PopupMenuItem::new(if holding {
                    "Recover…"
                } else {
                    "Resume recovery…"
                })
                .disabled(recovery_disabled)
                .on_click(move |_, window, cx| {
                    recover_view.update(cx, |view, cx| {
                        view.open_recovery(operation, None, window, cx);
                    });
                }),
            )
        })
        .separator()
        .item(
            PopupMenuItem::new(hide_label)
                .disabled(hide_disabled)
                .on_click(move |_, _, cx| {
                    hide_view.update(cx, |this, cx| {
                        if let Some(record) = this
                            .records
                            .iter()
                            .find(|record| record.operation() == operation)
                        {
                            if let Err(error) =
                                this.owner.set_hidden(operation, !record.is_hidden())
                            {
                                this.error = Some(error.to_string());
                            }
                            this.reload_records();
                            this.refresh_visible(cx);
                            cx.notify();
                        }
                    });
                }),
        )
    }

    pub(super) fn render_work_status(&self, cx: &Context<'_, Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(self.job.is_some(), |content| {
                content.child(
                    app_button("stealth-stop", "Stop")
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.stop_work(cx);
                        })),
                )
            })
            .children(
                self.coverage
                    .as_ref()
                    .map(|coverage| app_muted_text(coverage.clone()).whitespace_normal()),
            )
            .children(self.error.as_ref().map(|error| {
                app_strong_text(error.clone())
                    .text_color(cx.theme().danger)
                    .whitespace_normal()
            }))
    }
}

impl Render for StealthAccountsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let menu_view = cx.entity();
        let attention = self
            .records
            .iter()
            .filter(|record| self.needs_attention(record))
            .count();
        let holding = self
            .records
            .iter()
            .filter(|record| self.holding(record.operation()))
            .count();
        let more = app_button("stealth-more", "⋯")
            .ghost()
            .small()
            .accessibility_label("Stealth account actions")
            .tooltip("Stealth account actions")
            .dropdown_menu(move |menu, _, cx| {
                let view = menu_view.clone();
                let hidden_view = menu_view.clone();
                let show_hidden = menu_view.read(cx).show_hidden;
                menu.item(
                    PopupMenuItem::new("Restore…").on_click(move |_, window, cx| {
                        view.update(cx, |view, cx| view.open_restore(window, cx));
                    }),
                )
                .item(
                    PopupMenuItem::new("Show hidden")
                        .checked(show_hidden)
                        .on_click(move |_, _, cx| {
                            hidden_view.update(cx, |view, cx| {
                                view.show_hidden = !view.show_hidden;
                                view.pages.first();
                                view.refresh_visible(cx);
                                cx.notify();
                            });
                        }),
                )
            });
        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div().flex().flex_wrap().gap_1().children(
                    [
                        (
                            AccountFilter::Attention,
                            "Needs attention",
                            "attention",
                            attention,
                        ),
                        (
                            AccountFilter::Holding,
                            "Holding balance",
                            "holding",
                            holding,
                        ),
                        (
                            AccountFilter::Used,
                            "Used",
                            "used",
                            self.records
                                .iter()
                                .filter(|record| was_used(record))
                                .count(),
                        ),
                        (AccountFilter::All, "All", "all", self.records.len()),
                    ]
                    .into_iter()
                    .map(|(filter, label, id, count)| {
                        app_segment_button(
                            id,
                            format!("{label}  {count}"),
                            self.filter == filter,
                            false,
                            None,
                        )
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.filter = filter;
                            this.pages.first();
                            this.refresh_visible(cx);
                            cx.notify();
                        }))
                    }),
                ),
            )
            .child(
                div().w(rems(16.)).child(
                    app_input(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(div().flex_1())
            .when(self.show_hidden, |toolbar| {
                toolbar.child(account_caption("Including hidden"))
            })
            .child(more);
        let header = TableHeader::new().child(
            TableRow::new()
                .child(
                    TableHead::new()
                        .w(rems(8.))
                        .flex_none()
                        .child(account_caption("Account")),
                )
                .child(
                    TableHead::new()
                        .flex_1()
                        .min_w(rems(11.))
                        .child(account_caption("Used for")),
                )
                .child(
                    TableHead::new()
                        .w(rems(11.))
                        .flex_none()
                        .child(account_caption("Status")),
                )
                .child(
                    TableHead::new()
                        .flex_1()
                        .min_w(rems(8.))
                        .child(account_caption("Balance")),
                )
                .child(TableHead::new().w(rems(5.)).min_w_0().flex_none()),
        );
        let mut card = div()
            .debug_selector(|| "stealth-accounts-card".to_owned())
            .w_full()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md();
        if let Some(error) = &self.records_error {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_2()
                    .p_4()
                    .child(
                        app_strong_text("Saved accounts could not be loaded")
                            .text_color(cx.theme().danger),
                    )
                    .child(
                        account_caption(
                            "Your saved history has not been changed. Try loading it again.",
                        )
                        .whitespace_normal(),
                    )
                    .child(account_caption(error.clone()).whitespace_normal())
                    .child(
                        app_button("stealth-reload", "Try again")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_records();
                                this.refresh_visible(cx);
                                cx.notify();
                            })),
                    ),
            );
        }
        if self.records.is_empty() {
            if self.records_error.is_none() {
                card = card.child(div().flex().flex_col().items_start().gap_2().p_4()
                    .child(app_strong_text("No stealth accounts yet"))
                    .child(account_caption("Accounts appear here when a private operation executes through one. If this wallet was imported and used before, restore earlier accounts from their account index.").max_w(rems(36.)).whitespace_normal())
                    .child(app_button("stealth-restore-empty", "Restore…").outline().small().on_click(cx.listener(|this, _, window, cx| this.open_restore(window, cx)))));
            }
        } else {
            let rows = self.pages.accounts[self.pages.range()]
                .iter()
                .filter_map(|operation| self.render_row(*operation, cx))
                .collect();
            let checked = self
                .observations
                .values()
                .filter(|observations| {
                    observations
                        .assets
                        .values()
                        .any(|balance| balance.attempt != CheckAttempt::NotChecked)
                })
                .count();
            card = card
                .flex_1()
                .child(toolbar.flex_shrink_0())
                .child(account_table(&self.pages, header, rows))
                .child(
                    page_footer(
                        &self.pages,
                        self.records.len(),
                        checked,
                        cx.listener(|this, page, _, cx| {
                            this.pages.change(*page - 1);
                            cx.notify();
                        }),
                    )
                    .border_t_1()
                    .border_color(cx.theme().border),
                );
        }
        // BreadcrumbItem has no focus hook in the pinned component version.
        // The container exposes its one navigation command to the keyboard.
        let breadcrumb = div()
            .id("stealth-breadcrumb")
            .debug_selector(|| "stealth-breadcrumb".to_owned())
            .self_start()
            .track_focus(&self.breadcrumb_focus)
            .tab_index(0)
            .tab_stop(true)
            .role(gpui::Role::Link)
            .aria_label("Back to Public")
            .focus_visible(|style| style.bg(cx.theme().accent))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.return_to_public(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                Breadcrumb::new()
                    .line_height(gpui::relative(ui::theme::APP_TEXT_LINE_HEIGHT))
                    .child(
                        BreadcrumbItem::new("Public")
                            .on_click(cx.listener(|this, _, _, cx| this.return_to_public(cx))),
                    )
                    .child(BreadcrumbItem::new("Stealth accounts")),
            );
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(breadcrumb)
            .child(card)
            .child(self.render_work_status(cx))
    }
}

// Styled TableRow has no interaction hooks. This wrapper keeps its stock cells
// and sizing while adding summary-row expansion and a context-menu target.
#[derive(IntoElement)]
struct AccountMenuRow {
    row: TableRow,
    operation: ExecutorOperationId,
    view: Entity<StealthAccountsView>,
}

impl ChildElement for AccountMenuRow {
    fn with_ix(mut self, ix: usize) -> Self {
        self.row = self.row.with_ix(ix);
        self
    }
}

impl Sizable for AccountMenuRow {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.row = self.row.with_size(size);
        self
    }
}

impl RenderOnce for AccountMenuRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let operation = self.operation;
        let view = self.view.clone();
        div()
            .id(SharedString::from(format!(
                "account-row-{}",
                operation.opaque_id()
            )))
            .debug_selector(move || format!("stealth-row-{}", operation.opaque_id()))
            .w_full()
            .cursor_pointer()
            .child(self.row)
            .on_click(move |_, _, cx| {
                view.update(cx, |view, cx| view.toggle_expanded(operation, cx));
            })
            .context_menu(move |menu, _, cx| {
                StealthAccountsView::account_menu(&self.view, operation, menu, cx)
            })
    }
}

fn account_rows(
    summary: impl ChildElement + 'static,
    inspector: Option<gpui::Div>,
    cx: &App,
) -> TableBody {
    TableBody::new()
        .border_b_1()
        .border_color(cx.theme().table_row_border)
        .child(summary)
        .when_some(inspector, |body, inspector| {
            body.child(
                TableRow::new().child(
                    TableCell::new()
                        .col_span(5)
                        .w_full()
                        .p_0()
                        .child(inspector.w_full()),
                ),
            )
        })
}

fn account_table(
    pages: &AccountPages,
    header: TableHeader,
    rows: Vec<TableBody>,
) -> impl IntoElement {
    // Keep a bounded viewport so the inspector can wrap before the table scrolls.
    div()
        .id("stealth-account-table")
        .relative()
        .flex_1()
        .h_full()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .overflow_x_scroll()
        .lock_scroll_axis()
        .track_scroll(&pages.horizontal_scroll)
        .child(
            Table::new()
                .accessibility_label("Stealth accounts")
                .small()
                .h_full()
                .min_w(rems(43.))
                .flex()
                .flex_col()
                .child(header.flex_shrink_0())
                .child(AccountTableBody {
                    scroll: pages.scroll.clone(),
                    rows,
                    size: Size::default(),
                    ix: 0,
                }),
        )
        .horizontal_scrollbar(&pages.horizontal_scroll)
}

// The stock Table accepts typed children but exposes no scroll handle on TableBody.
// Keep one body group per account as a direct scroll child so reveal() still targets
// the account when an earlier account has a second, expanded inspector row.
#[derive(IntoElement)]
struct AccountTableBody {
    scroll: gpui::ScrollHandle,
    rows: Vec<TableBody>,
    size: Size,
    ix: usize,
}

impl Sizable for AccountTableBody {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl ChildElement for AccountTableBody {
    fn with_ix(mut self, ix: usize) -> Self {
        self.ix = ix;
        self
    }
}

impl RenderOnce for AccountTableBody {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let rows = if self.rows.is_empty() {
            vec![
                TableBody::new().child(
                    TableRow::new().child(
                        TableCell::new()
                            .col_span(5)
                            .child(account_caption("No accounts match these filters.")),
                    ),
                ),
            ]
        } else {
            self.rows
        };
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id(("stealth-page-rows", self.ix))
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .lock_scroll_axis()
                    .track_scroll(&self.scroll)
                    .children(
                        rows.into_iter().enumerate().map(|(ix, body)| {
                            body.with_ix(ix).with_size(self.size).flex_shrink_0()
                        }),
                    ),
            )
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::vertical(&self.scroll)
                        .mode(ScrollbarMode::Always)
                        .viewport_from_layout(),
                ),
            )
    }
}

fn page_footer(
    pages: &AccountPages,
    total: usize,
    checked: usize,
    on_page: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let range = pages.range();
    let count = pages.accounts.len();
    let summary = if count == total {
        format!("{count} accounts")
    } else {
        format!("{count} matching accounts · {total} total")
    };
    div()
        .px_3()
        .py_2()
        .flex_shrink_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(rems(16.))
                .child(account_caption(format!(
                    "{}–{} of {summary}",
                    if count == 0 { 0 } else { range.start + 1 },
                    range.end
                )))
                .child(account_caption(format!(
                    "{ACCOUNTS_PER_PAGE} per page · newest first · {checked} checked this session"
                ))),
        )
        .child(
            div()
                .flex_none()
                .debug_selector(|| "stealth-pagination".into())
                .child(
                    Pagination::new("stealth-pages")
                        .small()
                        .p_0()
                        .current_page(pages.page + 1)
                        .total_pages(pages.count())
                        .visible_pages(3)
                        .disabled(count == 0)
                        .on_click(on_page),
                ),
        )
}

pub(super) fn account_caption(label: impl Into<SharedString>) -> gpui::Div {
    app_muted_text(label).text_xs()
}

fn outcome_tag(outcome: ExecutorAccountOutcome) -> Tag {
    let tag = match outcome {
        ExecutorAccountOutcome::Executed => Tag::success(),
        ExecutorAccountOutcome::Reverted => Tag::danger(),
        ExecutorAccountOutcome::MissingEffects => Tag::warning(),
        ExecutorAccountOutcome::Unconfirmed | ExecutorAccountOutcome::RecoveryPending => {
            Tag::info()
        }
        _ => Tag::secondary(),
    };
    tag.outline()
        .small()
        .rounded_full()
        .line_height(gpui::relative(ui::theme::APP_TEXT_LINE_HEIGHT))
        .child(outcome_label(outcome))
}

pub(super) const fn outcome_label(outcome: ExecutorAccountOutcome) -> &'static str {
    match outcome {
        ExecutorAccountOutcome::NotSigned => "Not signed",
        ExecutorAccountOutcome::HistoryUnknown => "History unknown",
        ExecutorAccountOutcome::Unconfirmed => "Unconfirmed",
        ExecutorAccountOutcome::Executed => "Executed",
        ExecutorAccountOutcome::Reverted => "Reverted",
        ExecutorAccountOutcome::MissingEffects => "Effects missing",
        ExecutorAccountOutcome::RecoveryPending => "Recovery pending",
        ExecutorAccountOutcome::RecoveryConfirmed => "Recovery confirmed",
    }
}

fn observation_age(at: std::time::SystemTime) -> String {
    let minutes = at.elapsed().unwrap_or_default().as_secs() / 60;
    if minutes == 0 {
        "just checked".into()
    } else {
        format!("{minutes}m ago")
    }
}

pub(super) fn has_local_history(record: &ExecutorRecord) -> bool {
    record.origin() == ExecutorRecordOrigin::Reserved
        || !record.issued().is_empty()
        || !record.recovery_transactions().is_empty()
}

fn was_used(record: &ExecutorRecord) -> bool {
    record
        .use_check()
        .observation()
        .is_some_and(|observation| observation.was_used())
        || record
            .issued()
            .iter()
            .any(|payload| payload.inclusion().is_some())
        || record
            .recovery_transactions()
            .iter()
            .any(|transaction| transaction.inclusion().is_some())
}

const fn use_label(record: &ExecutorRecord) -> &'static str {
    let check = record.use_check();
    match check.observation() {
        Some(observation) if observation.was_used() => "Used",
        Some(_) => "Unused",
        None if check.is_unavailable() => "Check unavailable",
        None => "Not checked",
    }
}

pub(super) fn timestamp_label(at: u64) -> String {
    i64::try_from(at)
        .ok()
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
        .map_or_else(
            || "time unknown".into(),
            |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
}

struct AccountVisibility {
    attention: bool,
    unresolved: bool,
    holding: bool,
}

fn visible_accounts(
    records: &[ExecutorRecord],
    filter: AccountFilter,
    show_hidden: bool,
    search: &str,
    visibility: impl Fn(&ExecutorRecord) -> AccountVisibility,
    asset_names: impl Fn(&ExecutorRecord) -> String,
) -> Vec<ExecutorOperationId> {
    let mut records = records
        .iter()
        .filter(|record| {
            let visibility = visibility(record);
            (!record.is_hidden() || show_hidden || visibility.attention || visibility.unresolved)
                && match filter {
                    AccountFilter::All => true,
                    AccountFilter::Attention => visibility.attention,
                    AccountFilter::Holding => visibility.holding,
                    AccountFilter::Used => was_used(record),
                }
                && (search.is_empty()
                    || format!(
                        "{} {} {} {}",
                        record.index(),
                        record
                            .address()
                            .map_or_else(String::new, |address| address.to_string()),
                        record.purpose_summary().unwrap_or_default(),
                        asset_names(record)
                    )
                    .to_lowercase()
                    .contains(search))
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|record| {
        std::cmp::Reverse((
            record.created_at().or_else(|| record.restored_at()),
            record.index(),
        ))
    });
    records.into_iter().map(ExecutorRecord::operation).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn used_filter_includes_recorded_transactions_without_a_restore_check() {
        use alloy::{
            eips::BlockNumHash,
            primitives::{B256, Bytes, U256},
            rpc::types::TransactionRequest,
        };
        use wallet_ops::vault::{
            ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
            ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
        };

        let block = BlockNumHash::new(10, B256::repeat_byte(10));
        let hash = B256::repeat_byte(1);
        let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
            U256::ZERO,
            Address::ZERO,
            hash,
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(
                Bytes::new(),
                ExecutorNonceObservation::new(block, U256::ZERO),
                Vec::new(),
            ),
        ))
        .unwrap();
        payload["transaction_hashes"] = serde_json::json!([hash]);
        let recovery = serde_json::json!({
            "recovery": ExecutorOperationId::random().unwrap(),
            "step": 0, "kind": "Shield", "hash": hash, "observed": block,
            "transaction": TransactionRequest::default().nonce(0),
            "inclusion": null,
        });
        for (field, transaction) in [("issued", payload), ("recovery_transactions", recovery)] {
            let records = [
                None,
                Some(ExecutorExecutionResult::Executed),
                Some(ExecutorExecutionResult::Reverted),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                let mut record = serde_json::json!({
                    "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
                    "operation": ExecutorOperationId::random().unwrap(), "index": index,
                    "address": Address::ZERO, "delegate": Address::ZERO,
                    "retired": true, "issued": [], "recovery_transactions": [],
                });
                let mut transaction = transaction.clone();
                transaction["inclusion"] = serde_json::to_value(
                    result.map(|result| ExecutorPayloadInclusion::new(block, hash, result)),
                )
                .unwrap();
                record[field] = serde_json::json!([transaction]);
                // No Restore observation or current-session reconciliation is present.
                serde_json::from_value::<ExecutorRecord>(record).unwrap()
            })
            .collect::<Vec<_>>();
            let used = visible_accounts(
                &records,
                AccountFilter::Used,
                false,
                "",
                |_| AccountVisibility {
                    attention: false,
                    unresolved: false,
                    holding: false,
                },
                |_| String::new(),
            );
            assert_eq!(
                used,
                vec![records[2].operation(), records[1].operation()],
                "{field}: recorded use must count while an unconfirmed transaction must not"
            );
        }
    }

    #[test]
    fn large_collection_keeps_hidden_issues_findable_and_orders_known_times_before_index_fallback()
    {
        let records = (0..1_000_u32)
            .map(|index| {
                serde_json::from_value::<ExecutorRecord>(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().unwrap(), "index": index,
            "address": Address::ZERO, "delegate": Address::ZERO,
            "retired": true, "hidden": index % 2 == 0,
            "created_at": if index == 2 { Some(100) } else { None },
            "restored_at": if index == 4 { Some(200) } else { None },
            "purpose_summary": if index == 2 { "Unshield WETH" } else { "Earlier operation" },
            "assets": [ExecutorAsset::Native], "issued": [],
            "use_check": {
                "observation": (index <= 3 && index != 0).then(|| serde_json::json!({
                    "block": alloy::eips::BlockNumHash::new(10, alloy::primitives::B256::ZERO),
                    "checked_at": 200,
                    "nonce": (index != 3).then_some(alloy::primitives::U256::ZERO),
                })),
                "unavailable": index == 2,
            },
        })).unwrap()
            })
            .collect::<Vec<_>>();
        let visibility = |record: &ExecutorRecord| AccountVisibility {
            attention: record.index() == 2,
            unresolved: record.index() == 6,
            holding: record.index() == 2,
        };
        let used = visible_accounts(&records, AccountFilter::Used, false, "", visibility, |_| {
            String::new()
        });
        assert_eq!(
            used,
            vec![records[2].operation(), records[1].operation()],
            "Used includes decoded zero and a retained success after failure, excluding empty or absent evidence"
        );
        let all = visible_accounts(&records, AccountFilter::All, false, "", visibility, |_| {
            String::new()
        });
        assert_eq!(all.len(), 502);
        assert_eq!(all[0], records[2].operation());
        assert_eq!(all[1], records[999].operation());
        assert!(all.contains(&records[6].operation()));
        assert!(!all.contains(&records[4].operation()));
        let attention = visible_accounts(
            &records,
            AccountFilter::Attention,
            false,
            "weth",
            visibility,
            |_| String::new(),
        );
        assert_eq!(attention, vec![records[2].operation()]);
        assert_eq!(
            visible_accounts(
                &records,
                AccountFilter::Holding,
                false,
                "",
                visibility,
                |_| String::new()
            ),
            attention
        );
        let shown = visible_accounts(&records, AccountFilter::All, true, "", visibility, |_| {
            String::new()
        });
        assert_eq!(
            &shown[..2],
            &[records[4].operation(), records[2].operation()]
        );
        // An account-local token label is searchable even without a purpose match.
        let token = visible_accounts(
            &records,
            AccountFilter::All,
            true,
            "dai",
            visibility,
            |record| {
                if record.index() == 4 {
                    "DAI".into()
                } else {
                    String::new()
                }
            },
        );
        assert_eq!(token, vec![records[4].operation()]);
    }

    struct PagedAccountsHost {
        pages: AccountPages,
        total: usize,
        target: ExecutorOperationId,
        picked: std::rc::Rc<std::cell::Cell<Option<ExecutorOperationId>>>,
    }

    impl Render for PagedAccountsHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let range = self.pages.range();
            let rows = self.pages.accounts[range.clone()]
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    let operation = *id;
                    let picked = self.picked.clone();
                    account_rows(
                        TableRow::new().child(
                            TableCell::new().col_span(5).child(
                                app_button(
                                    SharedString::from(operation.opaque_id()),
                                    "Open account",
                                )
                                .when(index + 1 == range.len(), |button| {
                                    button.debug_selector(|| "page-last-account".into())
                                })
                                .when(operation == self.target, |button| {
                                    button.debug_selector(|| "page-target-account".into())
                                })
                                .on_click(move |_, _, _| picked.set(Some(operation))),
                            ),
                        ),
                        (index == 0).then(|| div().h(rems(12.)).child("Expanded inspector")),
                        cx,
                    )
                })
                .collect();
            div()
                .size_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(account_table(
                    &self.pages,
                    TableHeader::new().child(
                        TableRow::new().child(TableHead::new().col_span(5).child("Accounts")),
                    ),
                    rows,
                ))
                .child(page_footer(
                    &self.pages,
                    self.total,
                    0,
                    cx.listener(|this, page, _, cx| {
                        this.pages.change(*page - 1);
                        cx.notify();
                    }),
                ))
        }
    }

    #[gpui::test]
    fn paging_and_scrolling_keep_every_account_reachable(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use gpui_component::Root;
        cx.update(gpui_component::init);
        cx.update(ui::theme::apply_zenburn_component_theme);
        let accounts = (0..64)
            .map(|_| ExecutorOperationId::random().unwrap())
            .collect::<Vec<_>>();
        let picked = std::rc::Rc::new(std::cell::Cell::new(None));
        let (host, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| {
                let mut pages = AccountPages::default();
                pages.update(accounts.clone());
                PagedAccountsHost {
                    pages,
                    total: accounts.len(),
                    target: accounts[35],
                    picked: picked.clone(),
                }
            });
            Root::new(view, window, cx)
        });
        let host = host.read_with(cx, |root, _| {
            root.view().clone().downcast::<PagedAccountsHost>().unwrap()
        });
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for (page, last) in [(0, 24), (1, 49), (2, 63)] {
            if page != 0 {
                let controls = cx
                    .debug_bounds("stealth-pagination")
                    .expect("Page controls");
                cx.simulate_click(
                    gpui::point(controls.right() - gpui::px(20.), controls.center().y),
                    gpui::Modifiers::none(),
                );
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            assert_eq!(host.read_with(cx, |host, _| host.pages.page), page);
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::point(gpui::px(100.), gpui::px(250.)),
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-5000.))),
                ..Default::default()
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let last_row = cx
                .debug_bounds("page-last-account")
                .expect("Last account on the page");
            cx.simulate_click(last_row.center(), gpui::Modifiers::none());
            assert_eq!(
                picked.get(),
                Some(accounts[last]),
                "The page must scroll to its last account"
            );
        }
        // A recovery link also works in a narrow window with enlarged text.
        cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(420.)));
        cx.update(|window, cx| {
            window.set_rem_size(gpui::px(20.));
            host.update(cx, |host, cx| {
                host.pages.reveal(accounts[35]);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        let target = cx
            .debug_bounds("page-target-account")
            .expect("Recovery target");
        cx.simulate_click(target.center(), gpui::Modifiers::none());
        assert_eq!(picked.get(), Some(accounts[35]));
        // A narrower result set cannot leave an empty, out-of-range page.
        cx.update(|window, cx| {
            host.update(cx, |host, cx| {
                host.pages.update(vec![accounts[60]]);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        let only = cx
            .debug_bounds("page-last-account")
            .expect("Filtered account");
        cx.simulate_click(only.center(), gpui::Modifiers::none());
        assert_eq!(picked.get(), Some(accounts[60]));
        assert_eq!(host.read_with(cx, |host, _| host.pages.page), 0);
    }
}
