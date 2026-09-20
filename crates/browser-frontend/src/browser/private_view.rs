use super::*;
use gpui_component::progress::Progress;
use gpui_component::{Placement, button::ButtonGroup, select::SelectEvent, tooltip::Tooltip};
use public_view::array;

#[derive(Clone, PartialEq, Eq)]
struct PrivateWallet {
    wallet_id: String,
    label: String,
    hardware: Option<String>,
}

impl SelectItem for PrivateWallet {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone().into()
    }
    fn value(&self) -> &Self::Value {
        &self.wallet_id
    }
    fn display_title(&self) -> Option<AnyElement> {
        Some(
            ui::wallet_identity::wallet_label_row(self.label.clone(), self.hardware.as_deref())
                .into_any_element(),
        )
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.row()
    }
    fn matches(&self, query: &str) -> bool {
        self.label.to_lowercase().contains(&query.to_lowercase())
    }
}
impl PrivateWallet {
    fn row(&self) -> Div {
        ui::wallet_identity::wallet_label_row(self.label.clone(), self.hardware.as_deref())
    }
}

struct PrivateAsset {
    id: String,
    symbol: String,
    amount: String,
    usd: Option<String>,
    icon: Option<String>,
    pending_verification: Option<String>,
    pending_incoming: Option<String>,
    pending_outgoing: Option<String>,
}

#[derive(Default)]
pub(super) struct PrivateView {
    pub(super) supported: bool,
    pub(super) actions_supported: bool,
    pub(super) self_broadcast_supported: bool,
    pub(super) selected_wallet: Option<String>,
    selected_wallet_choice: Option<String>,
    receive_address: Option<String>,
    pub(super) selected_chain: Option<u64>,
    wallets: Vec<PrivateWallet>,
    pub(super) selection_message: Option<String>,
    state: String,
    message: Option<String>,
    stage_label: Option<String>,
    percent: Option<f32>,
    total: Option<String>,
    assets: Vec<PrivateAsset>,
    pending: Option<JsValue>,
}

impl PrivateView {
    pub(super) fn from_snapshot(snapshot: &JsValue) -> Self {
        if flag_field(snapshot, "locked") || !flag_field(snapshot, "private_view_supported") {
            return Self::default();
        }
        let value = field(snapshot, "private_view");
        let pending = field(&value, "pending");
        Self {
            supported: true,
            actions_supported: flag_field(snapshot, "private_actions_supported"),
            self_broadcast_supported: flag_field(snapshot, "private_self_broadcast_supported"),
            selected_wallet: field(&value, "selected_wallet").as_string(),
            selected_wallet_choice: field(&value, "selected_wallet_choice").as_string(),
            receive_address: field(&value, "receive_address").as_string(),
            selected_chain: chain_id_field(&value, "selected_chain"),
            wallets: array(&value, "wallets")
                .iter()
                .map(|wallet| PrivateWallet {
                    wallet_id: text_field(wallet, "wallet_id"),
                    label: text_field(wallet, "label"),
                    hardware: field(wallet, "hardware").as_string(),
                })
                .collect(),
            selection_message: field(&value, "selection_message").as_string(),
            state: text_field(&value, "state"),
            message: field(&value, "message").as_string(),
            stage_label: field(&value, "stage_label").as_string(),
            percent: field(&value, "percent").as_f64().map(|value| value as f32),
            total: field(&value, "total").as_string(),
            assets: array(&value, "assets")
                .iter()
                .map(|asset| PrivateAsset {
                    id: text_field(asset, "asset"),
                    symbol: text_field(asset, "symbol"),
                    amount: text_field(asset, "amount"),
                    usd: field(asset, "usd").as_string(),
                    icon: field(asset, "icon")
                        .as_string()
                        .filter(|path| path.starts_with("railgun-ui/")),
                    pending_verification: field(asset, "pending_verification").as_string(),
                    pending_incoming: field(asset, "pending_incoming").as_string(),
                    pending_outgoing: field(asset, "pending_outgoing").as_string(),
                })
                .collect(),
            pending: (!pending.is_null() && !pending.is_undefined()).then_some(pending),
        }
    }
    pub(super) fn draft_assets(&self) -> Vec<public_view::AssetBalance> {
        self.assets
            .iter()
            .map(|asset| public_view::AssetBalance {
                id: asset.id.clone(),
                symbol: asset.symbol.clone(),
                amount: asset.amount.clone(),
                max_amount: None,
                usd: asset.usd.clone().unwrap_or_default(),
                icon: asset.icon.clone().unwrap_or_default(),
            })
            .collect()
    }
    fn wallet(&self) -> Option<&PrivateWallet> {
        self.wallets
            .iter()
            .find(|wallet| Some(&wallet.wallet_id) == self.selected_wallet_choice.as_ref())
    }
}

pub(super) struct PrivateForm {
    wallets: Vec<PrivateWallet>,
    wallet: Entity<SelectState<SearchableVec<PrivateWallet>>>,
    _subscription: Subscription,
}

impl GatewayView {
    pub(super) fn sync_private_form(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.private_view.supported {
            self.private_form = None;
            return;
        }
        if self.private_form.is_none() {
            let wallet = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(Vec::<PrivateWallet>::new()),
                    None,
                    window,
                    cx,
                )
                .searchable(true)
            });
            let subscription = cx.subscribe_in(
                &wallet,
                window,
                |this, select, event: &SelectEvent<SearchableVec<PrivateWallet>>, window, cx| {
                    if let SelectEvent::Confirm(Some(wallet_id)) = event {
                        host_command(
                            "private_view",
                            &serde_json::json!({"type":"select_wallet", "wallet_id":wallet_id})
                                .to_string(),
                        );
                    }
                    select.update(cx, |select, cx| {
                        if let Some(wallet_id) = &this.private_view.selected_wallet_choice {
                            select.set_selected_value(wallet_id, window, cx);
                        } else {
                            select.set_selected_index(None, window, cx);
                        }
                    });
                },
            );
            self.private_form = Some(PrivateForm {
                wallets: Vec::new(),
                wallet,
                _subscription: subscription,
            });
        }
        let form = self.private_form.as_mut().expect("created private form");
        let changed = form.wallets != self.private_view.wallets;
        if changed {
            form.wallets.clone_from(&self.private_view.wallets);
            form.wallet.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(form.wallets.clone()), window, cx);
            });
        }
        if changed
            || form.wallet.read(cx).selected_value()
                != self.private_view.selected_wallet_choice.as_ref()
        {
            form.wallet.update(cx, |select, cx| {
                if let Some(wallet_id) = &self.private_view.selected_wallet_choice {
                    select.set_selected_value(wallet_id, window, cx);
                } else {
                    select.set_selected_index(None, window, cx);
                }
            });
        }
    }

    pub(super) fn render_wallet_picker(&self) -> Option<Div> {
        if !self.paired || self.status != "unlocked" {
            return None;
        }
        self.private_form.as_ref().map(|form| {
            div().flex_1().min_w_0().child(
                Select::new(&form.wallet)
                    .w_full()
                    .menu_width(rems(20.0))
                    .placeholder("Choose wallet")
                    .accessibility_label("Wallet")
                    .search_placeholder("Search wallets"),
            )
        })
    }

    pub(super) fn render_home_tabs(&self, cx: &Context<'_, Self>) -> ButtonGroup {
        let mut group = ButtonGroup::new("home-tabs").w_full().outline();
        for (tab, label, icon) in [
            ("private", "Private", ui::icons::shield_keyhole_icon_path()),
            ("public", "Public", ui::icons::eye_icon_path()),
        ] {
            group = group.child(
                ui::controls::app_segment_button(
                    tab,
                    label,
                    self.home_tab == tab,
                    tab == "private" && !self.selected_chain_has_railgun(),
                    None,
                )
                .when(
                    tab == "private" && !self.selected_chain_has_railgun(),
                    |button| {
                        button.tooltip("Private balances are unavailable on public-only chains")
                    },
                )
                .icon(Icon::empty().path(icon))
                .flex_1()
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.close_sheet(cx);
                    this.private_sheet = None;
                    if tab == "private" && !this.selected_chain_has_railgun() {
                        return;
                    }
                    this.home_tab = tab.into();
                    host_command("home_tab", tab);
                    cx.notify();
                })),
            );
        }
        group
    }

    fn private_receive_button(&self, cx: &Context<'_, Self>) -> Button {
        app_button("private-receive", "Receive")
            .outline()
            .icon(Icon::empty().path("ui/icons/qr-code.svg").small())
            .disabled(self.private_view.receive_address.is_none())
            .on_click(cx.listener(|this, _, window, cx| this.open_private_receive(window, cx)))
    }

    fn render_private_sync(&self) -> Div {
        let view = &self.private_view;
        let syncing = matches!(view.state.as_str(), "loading" | "syncing");
        let label = view
            .stage_label
            .as_ref()
            .or(view.message.as_ref())
            .cloned()
            .unwrap_or_else(|| "Preparing wallet sync".into());
        let caption_size = ui::wallet_balance::CAPTION_TEXT_SIZE;
        let bar_height = px(3.0);
        // Retain both rows when idle so sync and PPOI updates never move the content below.
        div()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .w_full()
                    .h(caption_size * theme::APP_TEXT_LINE_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(syncing, |row| {
                        let tooltip = label.clone();
                        row.child(
                            app_muted_text(label.clone())
                                .id("private-sync-stage")
                                .text_size(caption_size)
                                .flex_1()
                                .min_w_0()
                                .text_right()
                                .truncate()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tooltip.clone()).build(window, cx)
                                }),
                        )
                        .when_some(view.percent, |row, percent| {
                            row.child(
                                app_muted_text(format!("{percent:.0}%"))
                                    .text_size(caption_size)
                                    .flex_none(),
                            )
                        })
                    }),
            )
            .child(
                div()
                    .w_full()
                    .h(bar_height)
                    .flex_none()
                    .when(syncing, |bar| {
                        bar.child(
                            Progress::new("private-sync-progress")
                                .with_size(bar_height)
                                .value(view.percent.unwrap_or_default())
                                .loading(view.percent.is_none())
                                .accessibility_label(label),
                        )
                    }),
            )
    }

    pub(super) fn render_private_home(&self, cx: &Context<'_, Self>) -> Div {
        let view = &self.private_view;
        let mut body = div()
            .w_full()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.render_pending_requests(cx));
        if !self.ui_error.is_empty() {
            body = body.child(app_muted_text(self.ui_error.clone()));
        }
        body = body
            .child(ui::private_assets::private_balance(
                view.total.clone().unwrap_or_else(|| "Unavailable".into()),
                true,
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(ui::wallet_balance::wallet_balance_network(
                        div().w_full().children(
                            self.home_form
                                .as_ref()
                                .map(|form| chain_select(&form.chain).small().w_full()),
                        ),
                    ))
                    .child(self.render_private_sync()),
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(view.actions_supported, |row| {
                        row.child(
                            app_button("private-send", "Send")
                                .icon(
                                    Icon::empty()
                                        .path("ui/icons/arrow-big-right-dash.svg")
                                        .small(),
                                )
                                .outline()
                                .flex_1()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_private_draft("private_send", None, window, cx);
                                })),
                        )
                        .child(
                            app_button("private-unshield", "Unshield")
                                .icon(Icon::empty().path("ui/icons/shield.svg").small())
                                .outline()
                                .flex_1()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_private_draft("unshield", None, window, cx);
                                })),
                        )
                    })
                    .child(self.private_receive_button(cx).flex_1()),
            );
        let syncing = matches!(view.state.as_str(), "loading" | "syncing");
        if !syncing && let Some(message) = &view.message {
            body = body.child(
                ui::private_assets::private_message(message.clone(), None)
                    .h_auto()
                    .justify_start(),
            );
        }
        if view.wallet().is_some() && view.state == "ready" && view.assets.is_empty() {
            body = body.child(ui::private_assets::private_message("No private assets on this network. Receive private funds, or use Shield from Public to move funds into this wallet.", None).h_auto().justify_start());
        }
        if let Some(pending) = &view.pending {
            body = body.child(ui::private_assets::private_pending_status(
                text_field(pending, "title"),
                None,
                app_button("private-pending-details", "Details")
                    .ghost()
                    .small()
                    .compact()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_private_pending(window, cx)),
                    ),
            ));
        }
        body = body.children(view.assets.iter().map(|asset| {
            let asset_id = asset.id.clone();
            let row = ui::private_assets::PrivateAssetRow {
                compact: true,
                label: asset.symbol.clone().into(),
                icon: asset
                    .icon
                    .clone()
                    .map(|path| SharedString::from(path).into()),
                primary: asset.usd.as_ref().unwrap_or(&asset.amount).clone().into(),
                secondary: asset
                    .usd
                    .as_ref()
                    .map(|_| format!("{} {}", asset.amount, asset.symbol).into()),
                pending_verification: asset.pending_verification.clone().map(Into::into),
                pending_incoming: asset.pending_incoming.clone().map(Into::into),
                pending_outgoing: asset.pending_outgoing.clone().map(Into::into),
                actions: None,
            }
            .into_div();
            app_button_base(SharedString::from(format!(
                "private-send-asset-{}",
                asset.id
            )))
            .disabled(!view.actions_supported)
            .ghost()
            .w_full()
            .h_auto()
            .accessibility_label(format!("Send {}", asset.symbol))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_private_draft("private_send", Some(asset_id.clone()), window, cx);
            }))
            .child(row)
        }));
        body.child(
            div()
                .mt_auto()
                .pt_2()
                .child(summon_desktop_button().ghost().small()),
        )
    }

    fn open_private_receive(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(uuid) = self.private_view.selected_wallet.clone() else {
            return;
        };
        if self.private_view.receive_address.is_none() {
            return;
        }
        let Some(generation) = self.generation else {
            return;
        };
        self.private_sheet = Some((uuid.clone(), self.private_view.selected_chain));
        let view = cx.entity().downgrade();
        window.open_sheet_at(Placement::Top, cx, move |sheet, _, cx| {
            let receive = view.upgrade().and_then(|view| {
                let view = view.read(cx);
                if view.generation != Some(generation)
                    || view.private_view.selected_wallet.as_ref() != Some(&uuid)
                {
                    return None;
                }
                Some((
                    view.private_view.wallet()?.label.clone(),
                    view.private_view.receive_address.clone()?,
                ))
            });
            let Some((label, address)) = receive else {
                return sheet;
            };
            let copy_view = view.clone();
            let copy_uuid = uuid.clone();
            sheet
                .title("Private receive address")
                .resizable(false)
                .size(rems(36.0))
                .max_h_full()
                .child(ui::public_address::receive_address(
                    Some(label.into()),
                    address.into(),
                    None,
                    "copy-private-receive".into(),
                    px(3.0),
                    move |window, cx| {
                        let address = copy_view.upgrade().and_then(|view| {
                            let view = view.read(cx);
                            (view.generation == Some(generation)
                                && view.private_view.selected_wallet.as_ref() == Some(&copy_uuid))
                            .then(|| view.private_view.receive_address.clone())
                            .flatten()
                        });
                        if let Some(address) = address
                            && host_can_copy_address(
                                "private",
                                &chain_value(generation),
                                &copy_uuid,
                                &address,
                            )
                        {
                            ui::clipboard::copy_to_clipboard_with_toast(address, window, cx);
                        }
                    },
                ))
                .footer(
                    app_button("close-private-receive", "Close")
                        .outline()
                        .w_full()
                        .on_click(|_, window, cx| window.close_sheet(cx)),
                )
        });
    }

    fn open_private_pending(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(uuid) = self.private_view.selected_wallet.clone() else {
            return;
        };
        let chain = self.private_view.selected_chain;
        self.private_sheet = Some((uuid.clone(), chain));
        let view = cx.entity().downgrade();
        window.open_sheet_at(Placement::Top, cx, move |sheet, _, cx| {
            let pending = view.upgrade().and_then(|view| {
                let view = view.read(cx);
                (view.private_view.selected_wallet.as_ref() == Some(&uuid)
                    && view.private_view.selected_chain == chain)
                    .then(|| view.private_view.pending.clone())
                    .flatten()
            });
            let Some(pending) = pending else {
                return sheet;
            };
            let categories = array(&pending, "categories")
                .iter()
                .map(|category| ui::private_assets::PrivatePendingCategory {
                    title: text_field(category, "title").into(),
                    count: text_field(category, "count").into(),
                    detail: text_field(category, "detail").into(),
                    assets: array(category, "assets")
                        .iter()
                        .map(|asset| ui::private_assets::PrivatePendingAmount {
                            label: text_field(asset, "label").into(),
                            amount: text_field(asset, "amount").into(),
                            shield_wait: field(asset, "shield_wait").as_string().map(Into::into),
                        })
                        .collect(),
                })
                .collect();
            sheet
                .title(text_field(&pending, "title"))
                .resizable(false)
                .size(rems(42.0))
                .max_h_full()
                .children(field(&pending, "detail").as_string().map(app_muted_text))
                .child(ui::private_assets::private_pending_details(categories))
                .footer(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            app_button("close-private-pending", "Close")
                                .outline()
                                .flex_1()
                                .on_click(|_, window, cx| window.close_sheet(cx)),
                        )
                        .child(summon_desktop_button().flex_1()),
                )
        });
    }
}
