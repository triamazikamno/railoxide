use super::*;
use gpui::{Focusable as _, KeyBinding, Task, actions};
use gpui_component::{
    ActiveTheme as _, Placement,
    badge::Badge,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    select::SelectEvent,
    switch::Switch,
};
use serde_json::json;

actions!(
    gateway_view,
    [
        #[derive(Eq)]
        Back
    ]
);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Back, Some("GatewayView"))]);
}

fn command(value: &serde_json::Value) {
    host_command("public_view", &value.to_string());
}

#[derive(Default)]
pub(super) struct PublicView {
    pub selected_account: Option<String>,
    selected_chain: Option<u64>,
    balances: Vec<AccountBalances>,
    refreshing: bool,
    error: bool,
}
struct AccountBalances {
    uuid: String,
    total: String,
    assets: Vec<AssetBalance>,
}
struct AssetBalance {
    id: String,
    symbol: String,
    amount: String,
    usd: String,
    icon: String,
}
pub(super) struct SitePermission {
    id: String,
    origin: String,
    account: String,
    chain: u64,
}

fn array(value: &JsValue, key: &str) -> Vec<JsValue> {
    let value = field(value, key);
    if js_sys::Array::is_array(&value) {
        js_sys::Array::from(&value).iter().collect()
    } else {
        Vec::new()
    }
}
impl PublicView {
    pub(super) fn from_snapshot(snapshot: &JsValue) -> Self {
        if flag_field(snapshot, "locked") {
            return Self::default();
        }
        let value = field(snapshot, "public_view");
        Self {
            selected_account: field(&value, "selected_account").as_string(),
            selected_chain: chain_id_field(&value, "selected_chain"),
            refreshing: flag_field(&value, "refreshing"),
            error: flag_field(&value, "balance_error"),
            balances: array(&value, "balances")
                .iter()
                .map(|value| AccountBalances {
                    uuid: text_field(value, "account_uuid"),
                    total: text_field(value, "total"),
                    assets: array(value, "assets")
                        .iter()
                        .map(|value| AssetBalance {
                            id: text_field(value, "asset"),
                            symbol: text_field(value, "symbol"),
                            amount: text_field(value, "amount"),
                            usd: text_field(value, "usd"),
                            icon: text_field(value, "icon"),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
    fn balances(&self, uuid: &str) -> Option<&AccountBalances> {
        self.balances.iter().find(|balance| balance.uuid == uuid)
    }
}
pub(super) fn permissions(snapshot: &JsValue) -> Vec<SitePermission> {
    if flag_field(snapshot, "locked") {
        return Vec::new();
    }
    array(snapshot, "permissions")
        .iter()
        .filter_map(|value| {
            Some(SitePermission {
                id: text_field(value, "permission_id"),
                origin: text_field(value, "origin"),
                account: text_field(value, "account_uuid"),
                chain: chain_id_field(value, "chain_id")?,
            })
        })
        .collect()
}

pub(super) fn identicon(address: &str, cell_size: Rems) -> Div {
    let Ok(address) = address.parse::<alloy::primitives::Address>() else {
        return div();
    };
    let pattern = ui::public_address::public_account_identicon_pattern(address.as_ref());
    // Address-derived colors are part of the shared identicon's data encoding.
    let color = ui::public_address::public_account_identicon_color(address.as_ref());
    div()
        .flex()
        .flex_col()
        .flex_none()
        .children(pattern.as_chunks::<5>().0.iter().map(|row| {
            div().flex().flex_none().children(row.iter().map(|active| {
                div()
                    .size(cell_size)
                    .flex_none()
                    .when(*active, |cell| cell.bg(rgb(color)))
            }))
        }))
}
fn name(account: &ConnectAccount) -> String {
    if account.label.is_empty() {
        short_address(&account.address)
    } else {
        account.label.clone()
    }
}
fn chain_name(chains: &[ChainChoice], id: u64) -> String {
    chains
        .iter()
        .find(|chain| chain.id == id)
        .map_or_else(|| format!("Chain {id}"), |chain| chain.name.clone())
}
pub(super) fn chain_icon(id: u64) -> Div {
    div()
        .flex_none()
        .when_some(railgun_ui::chain_icon_asset_path(id), |this, path| {
            this.child(img(path).size_4())
        })
}

pub(super) struct HomeForm {
    accounts: Vec<ConnectAccount>,
    chains: Vec<ChainChoice>,
    account: Entity<SelectState<SearchableVec<AccountSelectItem>>>,
    chain: Entity<SelectState<SearchableVec<ChainSelectItem>>>,
    _subscriptions: Vec<Subscription>,
}

pub(super) struct AccountPicker {
    list: Entity<ListState<PickerList>>,
    _subscription: Subscription,
    return_focus: gpui::FocusHandle,
}
struct PickerRow {
    account: ConnectAccount,
    total: String,
    active: bool,
}
struct PickerList {
    rows: Vec<PickerRow>,
    matched: Vec<usize>,
    selected: Option<String>,
}
impl ListDelegate for PickerList {
    type Item = ListItem;
    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matched.len()
    }
    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_ascii_lowercase();
        self.matched = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.account.label.to_ascii_lowercase().contains(&query)
                    || row.account.address.to_ascii_lowercase().contains(&query)
                    || short_address(&row.account.address)
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .map(|(ix, _)| ix)
            .collect();
        let selected = self.selected.as_ref().and_then(|id| {
            self.matched
                .iter()
                .position(|ix| &self.rows[*ix].account.uuid == id)
        });
        cx.defer_in(window, move |list, window, cx| {
            list.set_selected_index(selected.map(|ix| IndexPath::default().row(ix)), window, cx);
        });
        Task::ready(())
    }
    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix
            .and_then(|ix| self.matched.get(ix.row))
            .map(|ix| self.rows[*ix].account.uuid.clone());
    }
    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<ListItem> {
        let row = self.rows.get(*self.matched.get(ix.row)?)?;
        Some(
            ListItem::new(SharedString::from(row.account.uuid.clone())).child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .w_full()
                    .py_1()
                    .child(identicon(&row.account.address, rems(0.4)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(app_text(name(&row.account)).truncate())
                                    .when(row.active, |this| {
                                        this.child(note("Active").text_color(cx.theme().primary))
                                    }),
                            )
                            .child(note(short_address(&row.account.address)).font_family(MONO)),
                    )
                    .child(note(row.total.clone()).flex_none()),
            ),
        )
    }
    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        note("No matching accounts")
    }
}

impl GatewayView {
    pub(super) fn clear_public_ui(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.close_sheet(cx);
        self.accounts.clear();
        self.public_view = PublicView::default();
        self.home_form = None;
        self.connect_form = None;
        self.picker = None;
        self.chains.clear();
        self.permissions.clear();
        self.current_tab.clear();
        self.ui_error.clear();
        self.sites_open = false;
        self.sites_show_all = false;
    }
    pub(super) fn sync_home_form(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.accounts.is_empty() || self.public_view.selected_chain.is_none() {
            self.home_form = None;
            return;
        }
        let new = self.home_form.is_none();
        if new {
            let account = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(Vec::<AccountSelectItem>::new()),
                    None,
                    window,
                    cx,
                )
                .searchable(true)
            });
            let chain = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(Vec::<ChainSelectItem>::new()),
                    None,
                    window,
                    cx,
                )
                .searchable(true)
            });
            let subscriptions = vec![
                cx.subscribe_in(
                    &account,
                    window,
                    |this,
                     select,
                     event: &SelectEvent<SearchableVec<AccountSelectItem>>,
                     window,
                     cx| {
                        if let SelectEvent::Confirm(Some(uuid)) = event {
                            command(
                                &json!({ "type": "select_account", "public_account_uuid": uuid }),
                            );
                        }
                        // The trigger stays on the confirmed desktop selection while the command travels.
                        if let Some(uuid) = this.public_view.selected_account.as_ref() {
                            select.update(cx, |select, cx| {
                                select.set_selected_value(uuid, window, cx);
                            });
                        }
                    },
                ),
                cx.subscribe_in(
                    &chain,
                    window,
                    |this,
                     select,
                     event: &SelectEvent<SearchableVec<ChainSelectItem>>,
                     window,
                     cx| {
                        if let SelectEvent::Confirm(Some(id)) = event {
                            command(&json!({ "type": "select_chain", "chain_id": id }));
                        }
                        if let Some(id) = this.public_view.selected_chain {
                            select.update(cx, |select, cx| {
                                select.set_selected_value(&id, window, cx);
                            });
                        }
                    },
                ),
            ];
            self.home_form = Some(HomeForm {
                accounts: Vec::new(),
                chains: Vec::new(),
                account,
                chain,
                _subscriptions: subscriptions,
            });
        }
        let form = self.home_form.as_mut().expect("created home form");
        let accounts_changed = form.accounts != self.accounts;
        if accounts_changed {
            form.accounts.clone_from(&self.accounts);
            let items: Vec<_> = self
                .accounts
                .iter()
                .map(|account| AccountSelectItem {
                    uuid: account.uuid.clone(),
                    label: account.label.clone(),
                    address: account.address.clone(),
                })
                .collect();
            form.account.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(items), window, cx);
            });
        }
        if let Some(uuid) = &self.public_view.selected_account
            && (accounts_changed || form.account.read(cx).selected_value() != Some(uuid))
        {
            form.account
                .update(cx, |select, cx| select.set_selected_value(uuid, window, cx));
        } else if self.public_view.selected_account.is_none()
            && form.account.read(cx).selected_value().is_some()
        {
            form.account
                .update(cx, |select, cx| select.set_selected_index(None, window, cx));
        }
        let chains_changed = form.chains != self.chains;
        if chains_changed {
            form.chains.clone_from(&self.chains);
            let items: Vec<_> = self
                .chains
                .iter()
                .map(|chain| ChainSelectItem {
                    id: chain.id,
                    name: chain.name.clone(),
                })
                .collect();
            form.chain.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(items), window, cx);
            });
        }
        if let Some(id) = &self.public_view.selected_chain
            && (chains_changed || form.chain.read(cx).selected_value() != Some(id))
        {
            form.chain
                .update(cx, |select, cx| select.set_selected_value(id, window, cx));
        }
        if new && !self.public_view.refreshing {
            command(&json!({ "type": "refresh_balances" }));
        }
    }
    fn active_account(&self) -> Option<&ConnectAccount> {
        self.accounts
            .iter()
            .find(|account| Some(&account.uuid) == self.public_view.selected_account.as_ref())
    }
    pub(super) fn navigate_back(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(picker) = self.picker.take() {
            picker.return_focus.focus(window, cx);
        } else {
            self.sites_open = false;
            self.focus.focus(window, cx);
        }
        cx.notify();
    }
    pub(super) fn render_sites_button(&self, cx: &Context<'_, Self>) -> Button {
        let mut permissions = self.permissions.iter().filter(|permission| {
            self.public_view.selected_account.as_ref() == Some(&permission.account)
        });
        let count = permissions.clone().count();
        let here = permissions.any(|permission| permission.origin == self.current_tab);
        app_button_base("connected-sites")
            .ghost()
            .small()
            .compact()
            .child(
                Badge::new()
                    .count(count)
                    .color(cx.theme().muted)
                    .child(Icon::new(IconName::Globe).size_4()),
            )
            .accessibility_label(if here {
                "Connected sites, this tab is connected"
            } else {
                "Connected sites"
            })
            .tooltip(if here {
                "Connected sites · this tab is connected"
            } else {
                "Connected sites"
            })
            .when(here, |button| {
                button.child(div().size_1().rounded_full().bg(cx.theme().success))
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.sites_open = !this.sites_open;
                this.sites_show_all = false;
                this.focus.focus(window, cx);
                cx.notify();
            }))
    }
    pub(super) fn render_home(&self, cx: &Context<'_, Self>) -> Div {
        let mut body = div()
            .flex()
            .flex_col()
            .w_full()
            .gap_4()
            .min_w_0()
            .flex_1()
            .children(self.render_pending_requests(cx));
        if !self.ui_error.is_empty() {
            body = body.child(note(self.ui_error.clone()));
        }
        if self.accounts.is_empty() {
            return body
                .child(note("No public accounts in the active wallet."))
                .child(summon_desktop_button());
        }
        let Some(form) = &self.home_form else {
            return body.child(note("Waiting for desktop account data."));
        };
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div().flex_1().min_w_0().child(
                        Select::new(&form.account)
                            .w_full()
                            .h_16()
                            .placeholder("Choose public account")
                            .accessibility_label("Public account")
                            .search_placeholder("Search accounts"),
                    ),
                )
                .when_some(self.active_account(), |this, account| {
                    this.child(
                        Clipboard::new("copy-home-address")
                            .value(account.address.clone())
                            .tooltip("Copy address"),
                    )
                }),
        );
        let Some(account) = self.active_account() else {
            return body.child(note("Choose an active public account to see its balances."));
        };
        let balances = self.public_view.balances(&account.uuid);
        body = body
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                app_strong_text(
                                    balances
                                        .filter(|balance| !balance.total.is_empty())
                                        .map_or("Unavailable", |balance| balance.total.as_str())
                                        .to_owned(),
                                )
                                .text_2xl()
                                .whitespace_nowrap(),
                            )
                            .child(note("Public balance")),
                    )
                    .child(
                        div().w_32().flex_none().child(
                            Select::new(&form.chain)
                                .small()
                                .w_full()
                                .accessibility_label("Network")
                                .search_placeholder("Search networks"),
                        ),
                    ),
            )
            .child(
                app_button("receive", "Receive…")
                    .outline()
                    .icon(IconName::ArrowDown)
                    .on_click(cx.listener(|this, _, window, cx| this.open_receive(window, cx))),
            );
        if self.public_view.error {
            body = body.child(note("Could not refresh balances. Try again."));
        }
        if let Some(balances) = balances {
            for asset in &balances.assets {
                body = body.child(
                    div()
                        .id(SharedString::from(format!("asset-{}", asset.id)))
                        .flex()
                        .items_center()
                        .gap_3()
                        .min_w_0()
                        .py_1()
                        .when(!asset.icon.is_empty(), |this| {
                            this.child(img(asset.icon.clone()).size_6().flex_none())
                        })
                        .child(
                            app_strong_text(asset.symbol.clone())
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .child(app_text(format!("{} {}", asset.amount, asset.symbol)))
                                .when(!asset.usd.is_empty(), |this| {
                                    this.child(note(asset.usd.clone()))
                                }),
                        ),
                );
            }
            if balances.assets.is_empty() {
                body = body.child(note(if balances.total.is_empty() {
                    "Balances unavailable. Refresh to load them."
                } else {
                    "No assets on this network."
                }));
            }
        }
        body.child(
            div()
                .mt_auto()
                .pt_2()
                .flex()
                .gap_2()
                .child(
                    app_button("refresh-balances", "Refresh")
                        .ghost()
                        .small()
                        .loading(self.public_view.refreshing)
                        .disabled(self.public_view.refreshing)
                        .on_click(|_, _, _| command(&json!({ "type": "refresh_balances" }))),
                )
                .child(div().flex_1())
                .child(summon_desktop_button().ghost().small()),
        )
    }
    fn open_receive(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(account) = self.active_account().cloned() else {
            return;
        };
        let view = cx.entity().downgrade();
        let uuid = account.uuid;
        // Re-read the live view in the sheet builder; it retains no address after lock.
        window.open_sheet_at(Placement::Top, cx, move |sheet, _, cx| {
            let account = view.upgrade().and_then(|view| {
                view.read(cx)
                    .accounts
                    .iter()
                    .find(|account| account.uuid == uuid)
                    .cloned()
            });
            let Some(account) = account else {
                return sheet;
            };
            sheet
                .title(format!("Receive to {}", name(&account)))
                .resizable(false)
                .size(rems(24.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_4()
                        // QR modules are raster dimensions, including the standard four-module quiet zone.
                        .child(ui::public_address::render_public_address_qr_code(
                            &account.address,
                            px(4.0),
                        ))
                        .child(
                            note(account.address.clone())
                                .font_family(MONO)
                                .whitespace_normal(),
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .w_full()
                        .gap_2()
                        .child(
                            app_button("close-receive", "Close")
                                .outline()
                                .flex_1()
                                .on_click(|_, window, cx| window.close_sheet(cx)),
                        )
                        .child(
                            app_button("copy-receive-address", "Copy address")
                                .primary()
                                .flex_1()
                                .icon(IconName::Copy)
                                .on_click(move |_, window, cx| {
                                    ui::clipboard::copy_to_clipboard_with_toast(
                                        account.address.clone(),
                                        window,
                                        cx,
                                    );
                                }),
                        ),
                )
        });
    }
    pub(super) fn render_sites(&self, cx: &Context<'_, Self>) -> Div {
        let mut permissions = self
            .permissions
            .iter()
            .filter(|permission| {
                self.sites_show_all
                    || self.public_view.selected_account.as_ref() == Some(&permission.account)
            })
            .peekable();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_w_0()
            .child(Self::back_title("Connected sites", cx))
            .child(
                Switch::new("show-all-site-accounts")
                    .small()
                    .label("Show all accounts")
                    .checked(self.sites_show_all)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.sites_show_all = *checked;
                        cx.notify();
                    })),
            );
        if !self.ui_error.is_empty() {
            body = body.child(note(self.ui_error.clone()));
        }
        if permissions.peek().is_none() {
            body = body
                .child(app_strong_text(if self.sites_show_all {
                    "No connected sites"
                } else {
                    "No sites connected to this account"
                }))
                .child(note(if self.permissions.is_empty() {
                    "Sites ask to connect when you use them. You can also start from here."
                } else {
                    "Turn on Show all accounts to manage this browser's other connections."
                }));
        }
        for permission in permissions {
            let account = self
                .accounts
                .iter()
                .find(|account| account.uuid == permission.account);
            let here = permission.origin == self.current_tab;
            let id = permission.id.clone();
            let mut row = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .min_w_0()
                .when(here, |this| this.bg(cx.theme().muted))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(
                            app_strong_text(permission.origin.clone())
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        )
                        .when(here, |this| this.child(note("This tab")))
                        .child(
                            app_button_base(SharedString::from(format!("revoke-{id}")))
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .tooltip("Revoke access")
                                .accessibility_label("Revoke access")
                                .on_click(move |_, _, _| {
                                    command(
                                        &json!({ "type": "revoke_permission", "permission_id": id }),
                                    );
                                }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .when_some(account, |this, account| {
                            this.child(identicon(&account.address, rems(0.2857)))
                                .child(note(name(account)).flex_1().min_w_0().truncate())
                                .child(note(short_address(&account.address)).font_family(MONO))
                        })
                        .when(account.is_none(), |this| {
                            this.child(note("Different wallet").flex_1())
                        })
                        .child(chain_icon(permission.chain))
                        .child(note(chain_name(&self.chains, permission.chain))),
                );
            if let Some(active) = self
                .active_account()
                .filter(|account| account.uuid != permission.account)
            {
                let id = permission.id.clone();
                let uuid = active.uuid.clone();
                row = row.child(app_button(SharedString::from(format!("reissue-{id}")), format!("Use {} here", name(active))).ghost().small()
                    .on_click(move |_, _, _| command(&json!({ "type": "reissue_permission", "permission_id": id, "public_account_uuid": uuid }))));
            }
            body = body.child(row);
        }
        if !self.current_tab.is_empty()
            && !self
                .permissions
                .iter()
                .any(|permission| permission.origin == self.current_tab)
        {
            body = body.child(
                app_button(
                    "connect-tab",
                    format!("Connect {}", site_host(&self.current_tab)),
                )
                .outline()
                .on_click(|_, _, _| command(&json!({ "type": "connect_tab" }))),
            );
        }
        body.child(note("Sites see only the account address. Signing and spending still need approval in the desktop app.").mt_auto())
    }
    fn back_title(title: &str, cx: &Context<'_, Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                app_button_base("back")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronLeft)
                    .tooltip("Back")
                    .accessibility_label("Back")
                    .on_click(cx.listener(|this, _, window, cx| this.navigate_back(window, cx))),
            )
            .child(app_strong_text(title.to_owned()))
    }
    pub(super) fn render_connect_account(&self, cx: &Context<'_, Self>) -> Div {
        let account = self
            .connect_form
            .as_ref()
            .and_then(|form| form.account.read(cx).selected_value())
            .and_then(|id| {
                self.connect_prompts
                    .first()?
                    .accounts
                    .iter()
                    .find(|account| &account.uuid == id)
            });
        let Some(account) = account else {
            return div().child(note("No public accounts in the active wallet"));
        };
        let active = self.public_view.selected_account.as_ref() == Some(&account.uuid);
        div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .p_3()
                .border_1()
                .border_color(cx.theme().border)
                .rounded_md()
                .child(identicon(&account.address, rems(0.5)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .flex_1()
                        .child(app_strong_text(name(account)).truncate())
                        .child(note(short_address(&account.address)).font_family(MONO))
                        .when(active, |this| {
                            this.child(note("Active").text_color(cx.theme().primary))
                        }),
                )
                .child(
                    app_button("change-connect-account", "Change")
                        .outline()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_account_picker(window, cx);
                        })),
                ),
        )
    }
    fn open_account_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(prompt) = self.connect_prompts.first() else {
            return;
        };
        let selected = self
            .connect_form
            .as_ref()
            .and_then(|form| form.account.read(cx).selected_value().cloned());
        let rows: Vec<_> = prompt
            .accounts
            .iter()
            .map(|account| PickerRow {
                account: account.clone(),
                total: self
                    .public_view
                    .balances(&account.uuid)
                    .map_or_else(String::new, |balance| balance.total.clone()),
                active: self.public_view.selected_account.as_ref() == Some(&account.uuid),
            })
            .collect();
        let selected_ix = selected
            .as_ref()
            .and_then(|id| rows.iter().position(|row| &row.account.uuid == id));
        let matched = (0..rows.len()).collect();
        let list = cx.new(|cx| {
            ListState::new(
                PickerList {
                    rows,
                    matched,
                    selected,
                },
                window,
                cx,
            )
            .searchable(true)
        });
        list.update(cx, |list, cx| {
            list.set_selected_index(
                selected_ix.map(|ix| IndexPath::default().row(ix)),
                window,
                cx,
            );
        });
        let subscription =
            cx.subscribe_in(&list, window, |this, _, event: &ListEvent, window, cx| {
                if matches!(event, ListEvent::Cancel) {
                    this.navigate_back(window, cx);
                } else {
                    cx.notify();
                }
            });
        let return_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        list.read(cx).focus_handle(cx).focus(window, cx);
        self.picker = Some(AccountPicker {
            list,
            _subscription: subscription,
            return_focus,
        });
        cx.notify();
    }
    pub(super) fn render_account_picker(&self, cx: &Context<'_, Self>) -> Div {
        let picker = self.picker.as_ref().expect("open picker");
        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(Self::back_title("Choose account", cx))
            .child(
                List::new(&picker.list)
                    .search_placeholder("Search accounts")
                    .flex_1()
                    .min_h_0(),
            )
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .gap_2()
                    .child(
                        app_button("cancel-picker", "Cancel")
                            .outline()
                            .flex_1()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.navigate_back(window, cx)),
                            ),
                    )
                    .child(
                        app_button("use-account", "Use account")
                            .primary()
                            .flex_1()
                            .disabled(picker.list.read(cx).delegate().selected.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let selected = this.picker.as_ref().and_then(|picker| {
                                    picker.list.read(cx).delegate().selected.clone()
                                });
                                if let Some(selected) = selected
                                    && let Some(form) = &this.connect_form
                                {
                                    form.account.update(cx, |account, cx| {
                                        account.set_selected_value(&selected, window, cx);
                                    });
                                    if this.public_view.selected_account.as_ref() != Some(&selected)
                                    {
                                        command(&json!({
                                            "type": "select_account",
                                            "public_account_uuid": selected,
                                        }));
                                    }
                                }
                                this.navigate_back(window, cx);
                            })),
                    ),
            )
    }
    pub(super) fn render_pending_requests(&self, cx: &Context<'_, Self>) -> Vec<Div> {
        self.pending_requests
            .iter()
            .map(|request| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().primary)
                    .bg(cx.theme().muted)
                    .child(app_strong_text(site_host(&request.url).to_owned()))
                    .child(note(if request.needs_unlock {
                        "Unlock the desktop app to review".to_owned()
                    } else {
                        request
                            .summary
                            .clone()
                            .unwrap_or_else(|| "Review in the desktop app".to_owned())
                    }))
                    .child(summon_desktop_button().small())
            })
            .collect()
    }
}
