use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
};

use gpui::{
    Anchor, AnyElement, App, AppContext as _, ApplicationHandle, AssetSource, Context, Div, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Rems, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window,
    WindowOptions, div, img, prelude::FluentBuilder as _, px, rems, rgb,
};
mod drafts;
mod keymap;
mod public_view;
use public_view::{HomeForm, PublicView, SitePermission};

use gpui_component::{
    Disableable as _, Icon, IconName, IndexPath, Root, Sizable as _, Theme, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    clipboard::Clipboard,
    input::{InputState, OtpInput, OtpState},
    menu::{DropdownMenu as _, PopupMenuItem},
    select::{SearchableVec, Select, SelectItem, SelectState},
    tag::Tag,
};
use ui::{
    controls::{
        app_button, app_button_base, app_button_label, app_input, app_muted_text, app_strong_text,
        app_text,
    },
    theme,
};
use wasm_bindgen::prelude::*;

const SANS: &str = "Inter Variable";
const MONO: &str = "JetBrains Mono";

thread_local! {
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static APPLICATION: RefCell<Option<ApplicationHandle>> = const { RefCell::new(None) };
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = isMac)]
    fn host_is_mac() -> bool;
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = isActive)]
    fn host_is_active() -> bool;
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = stage)]
    fn host_stage(stage: &str);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = ready)]
    fn host_ready();
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = fail)]
    fn host_fail(message: &str);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = subscribe)]
    fn host_subscribe(callback: &js_sys::Function);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = command)]
    fn host_command(command: &str, value: &str);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = pair)]
    fn host_pair(code: &str, endpoint: &str);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = subscribeConnect)]
    fn host_subscribe_connect(callback: &js_sys::Function);
    #[wasm_bindgen(js_namespace = railoxideHost, js_name = resolveConnect)]
    fn host_resolve_connect(request_id: &str, account: Option<&str>, chain_id: &JsValue);
}

struct BrowserAssets {
    preloaded: HashMap<String, Vec<u8>>,
    fallback: gpui_kit_assets::Assets,
}

impl AssetSource for BrowserAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if path == "railoxide/logo.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../bins/wallet/assets/icons/logo.svg"
            ))));
        }
        if path == "railoxide/wordmark.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../bins/wallet/assets/icons/wordmark.svg"
            ))));
        }
        if let Some(bytes) = self.preloaded.get(path) {
            return Ok(Some(Cow::Owned(bytes.clone())));
        }
        self.fallback.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        self.fallback.list(path)
    }
}

/// Starts the sole runtime for this document after the host has loaded local assets.
#[wasm_bindgen]
pub fn run(
    endpoint: String,
    inter: Vec<u8>,
    mono: Vec<u8>,
    icons: &js_sys::Object,
) -> Result<(), JsValue> {
    if !host_is_active() || STARTED.with(|started| started.replace(true)) {
        return Err(JsValue::from_str(
            "This document has already started or stopped. Reload it.",
        ));
    }
    // Copy the host's preflighted bytes before GPUI can request its first SVG.
    let mut preloaded = HashMap::new();
    for entry in js_sys::Object::entries(icons).iter() {
        let entry = js_sys::Array::from(&entry);
        let path = entry
            .get(0)
            .as_string()
            .ok_or_else(|| JsValue::from_str("Invalid packaged icon path."))?;
        let bytes = entry.get(1).dyn_into::<js_sys::Uint8Array>()?;
        preloaded.insert(path, bytes.to_vec());
    }
    gpui_kit::platform::web_init();
    host_stage("graphics initialization");
    let application = gpui_kit::platform::single_threaded_web().with_assets(BrowserAssets {
        preloaded,
        fallback: gpui_kit_assets::Assets::new(endpoint),
    });
    let handle = application.run_embedded(move |cx| {
        // Graphics preparation is asynchronous. A timeout or pagehide is terminal.
        if !host_is_active() {
            return;
        }
        gpui_kit::init(cx);
        keymap::init(cx, host_is_mac());
        theme::apply_zenburn_component_theme(cx);
        if cx
            .text_system()
            .add_fonts(vec![Cow::Owned(inter), Cow::Owned(mono)])
            .is_err()
        {
            host_fail("Packaged font registration failed. Rebuild the extension and reload it.");
            return;
        }
        let component_theme = Theme::global_mut(cx);
        component_theme.font_family = SANS.into();
        component_theme.mono_font_family = MONO.into();
        component_theme.font_size = theme::APP_TEXT_SIZE;
        // Extension views have no native title bar.
        component_theme.sheet.margin_top = px(0.0);
        Theme::sync_base(cx);
        host_stage("first rendered frame");
        if cx
            .open_window(WindowOptions::default(), |window, cx| {
                let gateway = cx.new(|cx| GatewayView::new(window, cx));
                window.on_next_frame(|_, _| {
                    if host_is_active() {
                        host_ready();
                    }
                });
                let view = cx.new(|_| GatewayWindow { gateway });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .is_err()
        {
            host_fail("The GPUI window could not open. Reload, or rebuild the extension.");
        }
    });
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(handle));
    Ok(())
}

/// Releases the document's application without invoking the web platform's no-op quit.
#[wasm_bindgen]
pub fn stop() {
    APPLICATION.with(|slot| {
        slot.borrow_mut().take();
    });
}

// These are desktop-supplied presentation choices. The desktop validates and commits grants.
#[derive(Clone, PartialEq, Eq)]
struct ConnectAccount {
    uuid: String,
    label: String,
    address: String,
}

#[derive(Clone, PartialEq, Eq)]
struct ChainChoice {
    id: u64,
    name: String,
}

struct ConnectPrompt {
    request_id: String,
    url: String,
    needs_unlock: bool,
    wrong_wallet: bool,
    accounts: Vec<ConnectAccount>,
    chains: Vec<ChainChoice>,
    default_chain_id: Option<u64>,
}

/// The account and network the connect screen offers, rebuilt whenever the prompt's choices change.
struct ConnectRequestForm {
    request_id: String,
    account_uuids: Vec<String>,
    chain_ids: Vec<u64>,
    account: Entity<SelectState<SearchableVec<AccountSelectItem>>>,
    chain: Entity<SelectState<SearchableVec<ChainSelectItem>>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, PartialEq, Eq)]
struct AccountSelectItem {
    uuid: String,
    label: String,
    address: String,
}

impl SelectItem for AccountSelectItem {
    type Value = String;

    fn title(&self) -> SharedString {
        SharedString::from(if self.label.is_empty() {
            short_address(&self.address)
        } else {
            self.label.clone()
        })
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(account_select_row(self).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        account_select_row(self)
    }

    fn value(&self) -> &Self::Value {
        &self.uuid
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        query.is_empty()
            || self.label.to_ascii_lowercase().contains(&query)
            || self.address.to_ascii_lowercase().contains(&query)
            || short_address(&self.address)
                .to_ascii_lowercase()
                .contains(&query)
    }
}

fn account_select_row(item: &AccountSelectItem) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .w_full()
        .child(public_view::identicon(&item.address, rems(0.45)))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(app_button_label(item.title()).truncate())
                .when(!item.label.is_empty(), |this| {
                    this.child(
                        app_button_label(short_address(&item.address))
                            .font_family(MONO)
                            .text_color(rgb(theme::TEXT_MUTED)),
                    )
                }),
        )
}

#[derive(Clone)]
struct ChainSelectItem {
    id: u64,
    name: String,
}

impl SelectItem for ChainSelectItem {
    type Value = u64;

    fn title(&self) -> SharedString {
        SharedString::from(self.name.clone())
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(public_view::chain_icon(self.id))
                .child(self.name.clone())
                .into_any_element(),
        )
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(public_view::chain_icon(self.id))
            .child(self.name.clone())
    }
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

struct PendingRequest {
    url: String,
    needs_unlock: bool,
    summary: Option<String>,
}

fn pending_requests(snapshot: &JsValue) -> Vec<PendingRequest> {
    let values = field(snapshot, "pending_requests");
    if !js_sys::Array::is_array(&values) {
        return Vec::new();
    }
    let locked = field(snapshot, "locked").as_bool().unwrap_or(true);
    js_sys::Array::from(&values)
        .iter()
        .map(|value| {
            let needs_unlock = locked || field(&value, "needs_unlock").as_bool().unwrap_or(true);
            PendingRequest {
                url: text_field(&value, "url"),
                needs_unlock,
                summary: if needs_unlock {
                    None
                } else {
                    field(&value, "summary").as_string()
                },
            }
        })
        .collect()
}

fn field(value: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(value, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn text_field(value: &JsValue, key: &str) -> String {
    field(value, key).as_string().unwrap_or_default()
}

fn flag_field(value: &JsValue, key: &str) -> bool {
    field(value, key).as_bool().unwrap_or(false)
}

/// Chain ids cross the host boundary as JSON numbers; the desktop only sends non-negative integers.
#[allow(clippy::cast_sign_loss)]
fn chain_id_field(value: &JsValue, key: &str) -> Option<u64> {
    field(value, key).as_f64().map(|id| id.max(0.0) as u64)
}

#[allow(clippy::cast_precision_loss)]
fn chain_value(chain_id: u64) -> JsValue {
    JsValue::from_f64(chain_id as f64)
}

fn chain_field(value: &JsValue, key: &str) -> Vec<ChainChoice> {
    let values = field(value, key);
    if !js_sys::Array::is_array(&values) {
        return Vec::new();
    }
    js_sys::Array::from(&values)
        .iter()
        .map(|chain| ChainChoice {
            id: chain_id_field(&chain, "id").unwrap_or_default(),
            name: text_field(&chain, "name"),
        })
        .collect()
}

fn account_field(value: &JsValue, key: &str) -> Vec<ConnectAccount> {
    let values = field(value, key);
    if !js_sys::Array::is_array(&values) {
        return Vec::new();
    }
    js_sys::Array::from(&values)
        .iter()
        .map(|account| ConnectAccount {
            uuid: text_field(&account, "uuid"),
            label: text_field(&account, "label"),
            address: text_field(&account, "address"),
        })
        .collect()
}

/// The active wallet's accounts, disclosed by the desktop only while it is unlocked.
fn wallet_accounts(snapshot: &JsValue) -> Vec<ConnectAccount> {
    if field(snapshot, "locked").as_bool().unwrap_or(true) {
        return Vec::new();
    }
    account_field(snapshot, "accounts")
}

fn connect_prompts(snapshot: &JsValue) -> Vec<ConnectPrompt> {
    let values = field(snapshot, "pending_connects");
    if !js_sys::Array::is_array(&values) {
        return Vec::new();
    }
    js_sys::Array::from(&values)
        .iter()
        .map(|value| {
            let needs_unlock = field(&value, "needs_unlock").as_bool().unwrap_or(true);
            let accounts = if needs_unlock {
                Vec::new()
            } else {
                account_field(&value, "accounts")
            };
            ConnectPrompt {
                request_id: text_field(&value, "request_id"),
                url: text_field(&value, "url"),
                needs_unlock,
                wrong_wallet: field(&value, "wrong_wallet").as_bool().unwrap_or(false),
                accounts,
                chains: chain_field(&value, "chains"),
                default_chain_id: chain_id_field(&value, "default_chain_id"),
            }
        })
        .collect()
}

fn pairing_failure(status: &str) -> Option<&'static str> {
    match status {
        "auth_failed" => {
            Some("The desktop app rejected this code. Generate a new one and try again.")
        }
        "version_failed" => {
            Some("Incompatible versions. Update the desktop app and the extension together.")
        }
        "config_failed" => Some("Endpoint or browser permission rejected. Check the host."),
        "storage_failed" => Some("Could not store the pairing. Reload the extension."),
        _ => None,
    }
}

fn unreachable_reason(status: &str) -> (&'static str, &'static str) {
    match status {
        "auth_failed" => (
            "Pairing rejected",
            "The desktop app no longer accepts this pairing. Pair again.",
        ),
        "version_failed" => (
            "Incompatible versions",
            "Update the desktop app and the extension together.",
        ),
        "config_failed" => (
            "Endpoint rejected",
            "Check the host and the browser permission.",
        ),
        "storage_failed" => ("Storage failed", "Reload the extension and pair again."),
        _ => (
            "Desktop app not reachable",
            "Open the desktop app and enable the browser gateway, or check the endpoint.",
        ),
    }
}

/// Mirrors the desktop's address shortening on the already formatted string.
fn short_address(address: &str) -> String {
    let characters: Vec<char> = address.chars().collect();
    if characters.len() <= 11 {
        return address.to_owned();
    }
    let head: String = characters[..6].iter().collect();
    let tail: String = characters[characters.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// The recognizable part of a URL: everything before the path, query, or fragment.
fn site_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() { url } else { host }
}

fn note(text: impl Into<SharedString>) -> Div {
    app_muted_text(text)
        .text_size(px(12.0))
        .line_height(px(17.0))
}

fn tag_color(color: u32, alpha: f32) -> gpui::Rgba {
    let mut value = rgb(color);
    value.a = alpha;
    value
}

struct GatewayView {
    code: Entity<OtpState>,
    endpoint: Entity<InputState>,
    status: String,
    configured_endpoint: String,
    seeded_endpoint: String,
    paired: bool,
    takeover: bool,
    metamask: bool,
    view: String,
    view_notice: String,
    accounts: Vec<ConnectAccount>,
    public_view: PublicView,
    draft: Option<drafts::DraftSnapshot>,
    draft_form: Option<drafts::DraftForm>,
    handoff_open: bool,
    draft_completion: Option<gpui::Task<()>>,
    completed_draft: Option<String>,
    cancel_draft_on_arrival: Option<String>,
    home_form: Option<HomeForm>,
    chains: Vec<ChainChoice>,
    permissions: Vec<SitePermission>,
    current_tab: String,
    ui_error: String,
    sites_open: bool,
    sites_show_all: bool,
    picker: Option<public_view::AccountPicker>,
    focus: gpui::FocusHandle,
    generation: Option<u64>,
    connect_prompts: Vec<ConnectPrompt>,
    connect_form: Option<ConnectRequestForm>,
    pending_requests: Vec<PendingRequest>,
    _callback: Closure<dyn FnMut(JsValue)>,
    _connect_callback: Closure<dyn FnMut(JsValue)>,
    _subscriptions: Vec<Subscription>,
}

impl GatewayView {
    fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        public_view::init(cx);
        let focus = cx.focus_handle();
        let code = cx.new(|cx| OtpState::new(6, window, cx));
        let endpoint = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1:43110"));
        let handle = window.window_handle();
        let view = cx.entity().downgrade();
        let mut app = cx.to_async();
        let callback = Closure::new(move |state: JsValue| {
            let status = text_field(&state, "status");
            let configured_endpoint = text_field(&state, "endpoint");
            let paired = flag_field(&state, "paired");
            let takeover = flag_field(&state, "takeover");
            let metamask = flag_field(&state, "metamask");
            let selected_view = text_field(&state, "view");
            let view_notice = text_field(&state, "view_notice");
            // The window is needed to seed the endpoint field from the host's saved value.
            let _ = app.update_window(handle, |_, window, cx| {
                let _ = view.update(cx, |view, cx| {
                    if status != "unlocked" && status != "locked" {
                        view.connect_prompts.clear();
                        view.pending_requests.clear();
                        if status != "paired" {
                            view.connect_form = None;
                        }
                    } else if status == "locked" {
                        for prompt in &mut view.connect_prompts {
                            prompt.needs_unlock = true;
                            prompt.accounts.clear();
                        }
                        for request in &mut view.pending_requests {
                            request.needs_unlock = true;
                            request.summary = None;
                        }
                    }
                    if status != "unlocked" {
                        view.clear_public_ui(window, cx);
                    }
                    view.status = status;
                    view.configured_endpoint = configured_endpoint;
                    view.paired = paired;
                    view.takeover = takeover;
                    view.metamask = metamask;
                    view.view = selected_view;
                    view.view_notice = view_notice;
                    // Follow the saved endpoint until the user edits the field.
                    let untouched = view.endpoint.read(cx).value().as_ref() == view.seeded_endpoint;
                    if untouched && view.seeded_endpoint != view.configured_endpoint {
                        view.seeded_endpoint = view.configured_endpoint.clone();
                        let saved = view.configured_endpoint.clone();
                        view.endpoint
                            .update(cx, |input, cx| input.set_value(saved, window, cx));
                    }
                    cx.notify();
                });
            });
        });
        host_subscribe(callback.as_ref().unchecked_ref());
        let view = cx.entity().downgrade();
        let mut app = cx.to_async();
        let connect_callback = Closure::new(move |snapshot: JsValue| {
            let prompts = connect_prompts(&snapshot);
            let requests = pending_requests(&snapshot);
            let accounts = wallet_accounts(&snapshot);
            let presentation = PublicView::from_snapshot(&snapshot);
            let draft = drafts::DraftSnapshot::from_snapshot(&snapshot);
            let chains = chain_field(&snapshot, "chains");
            let permissions = public_view::permissions(&snapshot);
            let current_tab = text_field(&snapshot, "current_tab_origin");
            let ui_error = text_field(&snapshot, "ui_error");
            let locked = flag_field(&snapshot, "locked");
            let generation = chain_id_field(&snapshot, "generation");
            // The window is needed to build the connect screen's select states.
            let _ = app.update_window(handle, |_, window, cx| {
                let _ = view.update(cx, |view, cx| {
                    if locked || view.generation != generation {
                        let keep_handoff = !locked
                            && view.handoff_open
                            && view
                                .draft
                                .as_ref()
                                .zip(draft.as_ref())
                                .is_some_and(|(previous, next)| previous.continues(next));
                        view.clear_public_ui(window, cx);
                        view.handoff_open = keep_handoff;
                    }
                    view.generation = generation;
                    view.public_view = presentation;
                    view.chains = chains;
                    view.permissions = permissions;
                    view.current_tab = current_tab;
                    view.ui_error = ui_error;
                    view.connect_prompts = prompts;
                    view.pending_requests = requests;
                    view.accounts = accounts;
                    view.sync_home_form(window, cx);
                    view.sync_draft(draft, window, cx);
                    view.sync_connect_form(window, cx);
                    cx.notify();
                });
            });
        });
        host_subscribe_connect(connect_callback.as_ref().unchecked_ref());
        let subscriptions = vec![
            cx.observe(&code, |_, _, cx| cx.notify()),
            cx.observe(&endpoint, |_, _, cx| cx.notify()),
        ];
        Self {
            code,
            endpoint,
            status: "disconnected".into(),
            configured_endpoint: String::new(),
            seeded_endpoint: String::new(),
            paired: false,
            takeover: false,
            metamask: false,
            view: "popup".into(),
            view_notice: String::new(),
            accounts: Vec::new(),
            public_view: PublicView::default(),
            draft: None,
            draft_form: None,
            handoff_open: false,
            draft_completion: None,
            completed_draft: None,
            cancel_draft_on_arrival: None,
            home_form: None,
            chains: Vec::new(),
            permissions: Vec::new(),
            current_tab: String::new(),
            ui_error: String::new(),
            sites_open: false,
            sites_show_all: false,
            picker: None,
            focus,
            generation: None,
            connect_prompts: Vec::new(),
            connect_form: None,
            pending_requests: Vec::new(),
            _callback: callback,
            _connect_callback: connect_callback,
            _subscriptions: subscriptions,
        }
    }

    /// Transport wording, independent of the desktop's lock state.
    fn transport_tag(&self) -> (&'static str, u32) {
        match (self.paired, self.status.as_str()) {
            (false, "connecting") => ("Pairing", theme::WARNING),
            (false, _) => ("Not paired", theme::TEXT_MUTED),
            (true, "connecting" | "disconnected") => ("Reconnecting", theme::WARNING),
            (true, "paired" | "locked" | "unlocked") => ("Connected", theme::SUCCESS),
            (true, _) => ("Disconnected", theme::TEXT_MUTED),
        }
    }

    fn render_header(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let (label, color) = self.transport_tag();
        let selected_view = self.view.clone();
        let takeover = self.takeover;
        let metamask = self.metamask;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .child(img("railoxide/logo.svg").size_8().flex_none())
            .child(
                div().flex_1().min_w_0().child(
                    img("railoxide/wordmark.svg")
                        .w_full()
                        .max_w(px(154.0))
                        .h(px(21.3))
                        .object_fit(gpui::ObjectFit::Contain),
                ),
            )
            .child(
                div().flex().flex_none().child(
                    Tag::custom(
                        tag_color(color, 0.12).into(),
                        rgb(color).into(),
                        rgb(color).into(),
                    )
                    .text_sm()
                    .rounded_full()
                    .child(label),
                ),
            )
            .when(self.status == "unlocked", |this| {
                this.child(self.render_sites_button(cx))
            })
            .child(
                Button::new("gateway-settings")
                    .ghost()
                    .small()
                    .compact()
                    .flex_none()
                    .child(Icon::new(IconName::Settings).size_5())
                    .accessibility_label("Settings")
                    .tooltip("Settings")
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _window, _cx| {
                        menu.min_w(px(200.0))
                            .item(
                                PopupMenuItem::new("Open as popup")
                                    .checked(selected_view == "popup")
                                    .on_click(|_, _, _| host_command("view", "popup")),
                            )
                            .item(
                                PopupMenuItem::new("Open as side panel")
                                    .checked(selected_view == "sidepanel")
                                    .on_click(|_, _, _| host_command("view", "sidepanel")),
                            )
                            .item(PopupMenuItem::separator())
                            .item(
                                PopupMenuItem::new("Use window.ethereum")
                                    .checked(takeover)
                                    .on_click(move |_, _, _| {
                                        host_command(
                                            "takeover",
                                            if takeover { "false" } else { "true" },
                                        );
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("Appear as MetaMask")
                                    .checked(metamask)
                                    .on_click(move |_, _, _| {
                                        host_command(
                                            "metamask",
                                            if metamask { "false" } else { "true" },
                                        );
                                    }),
                            )
                    }),
            )
    }

    /// The saved endpoint. Blank means the desktop app on this computer.
    fn render_endpoint(&self, cx: &Context<'_, Self>, reconnect: bool) -> Div {
        let block = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(note("Endpoint"))
            .min_w_0();
        let input = app_input(&self.endpoint).font_family(MONO);
        if reconnect {
            block.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(div().flex_1().min_w_0().child(input))
                    .child(
                        app_button("reconnect", "Reconnect")
                            .flex_none()
                            .on_click(cx.listener(|this, _, _, cx| {
                                host_command("configure", this.endpoint.read(cx).value().as_ref());
                            })),
                    ),
            )
        } else {
            block.child(input)
        }
    }

    fn render_pairing(&self, cx: &Context<'_, Self>) -> Div {
        let pairing = self.status == "connecting";
        let complete = {
            let code = self.code.read(cx).value();
            code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit())
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_strong_text("Pair with the desktop app"))
                    .child(note("Enter the code shown by the desktop app.")),
            )
            .child(
                div()
                    .flex()
                    .justify_center()
                    .py_2()
                    .child(OtpInput::new(&self.code).groups(2)),
            )
            .when_some(pairing_failure(&self.status), |this, message| {
                this.child(Alert::error("pairing-error", message).small())
            })
            .child(self.render_endpoint(cx, false))
            .child(
                app_button("pair", if pairing { "Pairing…" } else { "Pair" })
                    .primary()
                    .w_full()
                    .disabled(pairing || !complete)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let code = this.code.read(cx).value().to_string();
                        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                            return;
                        }
                        let endpoint = this.endpoint.read(cx).value();
                        host_pair(&code, endpoint.as_ref());
                        this.code
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    })),
            )
    }

    fn render_unreachable(&self, cx: &Context<'_, Self>) -> Div {
        let (title, description) = unreachable_reason(&self.status);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_strong_text(title))
                    .child(note(description)),
            )
            .child(self.render_endpoint(cx, true))
            .child(
                div().mt_auto().flex().justify_end().child(
                    app_button("unpair", "Pair again")
                        .outline()
                        .small()
                        .on_click(|_, _, _| host_command("unpair", "")),
                ),
            )
    }

    fn render_locked(&self, cx: &Context<'_, Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_w_0()
            .children(self.render_pending_requests(cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_center()
                    .pb_12()
                    .child(app_strong_text("Desktop app is locked"))
                    .child(note("Unlock it to see your accounts."))
                    .child(summon_desktop_button()),
            )
    }

    fn render_connected(&self, cx: &Context<'_, Self>) -> Div {
        if self.sites_open {
            self.render_sites(cx)
        } else if self.handoff_open {
            self.render_draft_handoff(cx)
        } else if self.draft_form.is_some() {
            self.render_draft_form(cx)
        } else {
            self.render_home(cx)
        }
    }

    /// Rebuilds the connect screen's selects whenever the leading prompt offers different choices.
    fn sync_connect_form(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(prompt) = self.connect_prompts.first() else {
            self.connect_form = None;
            self.picker = None;
            return;
        };
        let account_uuids: Vec<String> = prompt
            .accounts
            .iter()
            .map(|account| account.uuid.clone())
            .collect();
        let chain_ids: Vec<u64> = prompt.chains.iter().map(|chain| chain.id).collect();
        if self.connect_form.as_ref().is_some_and(|form| {
            form.request_id == prompt.request_id
                && form.account_uuids == account_uuids
                && form.chain_ids == chain_ids
        }) {
            return;
        }
        self.picker = None;
        let request_id = prompt.request_id.clone();
        let accounts: Vec<AccountSelectItem> = prompt
            .accounts
            .iter()
            .map(|account| AccountSelectItem {
                uuid: account.uuid.clone(),
                label: account.label.clone(),
                address: account.address.clone(),
            })
            .collect();
        let chains: Vec<ChainSelectItem> = prompt
            .chains
            .iter()
            .map(|chain| ChainSelectItem {
                id: chain.id,
                name: chain.name.clone(),
            })
            .collect();
        let default_chain_id = prompt.default_chain_id;
        let account_index = (!accounts.is_empty()).then(|| {
            IndexPath::default().row(
                self.public_view
                    .selected_account
                    .as_ref()
                    .and_then(|id| accounts.iter().position(|account| &account.uuid == id))
                    .unwrap_or_default(),
            )
        });
        let chain_index = (!chains.is_empty()).then(|| {
            IndexPath::default().row(
                default_chain_id
                    .and_then(|id| chains.iter().position(|chain| chain.id == id))
                    .unwrap_or_default(),
            )
        });
        let account = cx.new(|cx| {
            SelectState::new(SearchableVec::new(accounts), account_index, window, cx)
                .searchable(true)
        });
        let chain = cx.new(|cx| {
            SelectState::new(SearchableVec::new(chains), chain_index, window, cx).searchable(true)
        });
        let subscriptions = vec![
            cx.observe(&account, |_, _, cx| cx.notify()),
            cx.observe(&chain, |_, _, cx| cx.notify()),
        ];
        self.connect_form = Some(ConnectRequestForm {
            request_id,
            account_uuids,
            chain_ids,
            account,
            chain,
            _subscriptions: subscriptions,
        });
    }

    /// Answers the leading prompt on the network the form shows, falling back to the desktop default.
    fn resolve_connect(&self, account: Option<&str>, cx: &App) {
        let Some(prompt) = self.connect_prompts.first() else {
            return;
        };
        let chain = self
            .connect_form
            .as_ref()
            .and_then(|form| form.chain.read(cx).selected_value().copied())
            .or(prompt.default_chain_id)
            .or_else(|| prompt.chains.first().map(|chain| chain.id))
            .unwrap_or_default();
        host_resolve_connect(&prompt.request_id, account, &chain_value(chain));
    }

    fn render_connect_request(&self, cx: &Context<'_, Self>) -> Div {
        let Some(prompt) = self.connect_prompts.first() else {
            return div();
        };
        if self.picker.is_some() && !prompt.needs_unlock {
            return self.render_account_picker(cx);
        }
        let queued = self.connect_prompts.len();
        let form = self.connect_form.as_ref();
        let ready = form.is_some_and(|form| {
            form.account.read(cx).selected_value().is_some()
                && form.chain.read(cx).selected_value().is_some()
        });
        let mut screen = div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(app_strong_text("Connection request"))
                    .when(queued > 1, |row| {
                        row.child(div().flex_1().min_w_0()).child(
                            note(format!("1 of {queued}")).text_color(rgb(theme::TEXT_SUBTLE)),
                        )
                    }),
            )
            .child(render_site(&prompt.url));
        if prompt.needs_unlock {
            screen = screen.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_center()
                    .pb_6()
                    .child(app_strong_text("Desktop app is locked"))
                    .child(note("Unlock it to choose an account.")),
            );
        } else {
            if prompt.wrong_wallet {
                let switch_request = prompt.request_id.clone();
                screen = screen
                    .child(
                        Alert::warning(
                            "connect-wrong-wallet",
                            "Access belongs to a different wallet. Switch wallets in the desktop app, or share an account from the active wallet.",
                        )
                        .small(),
                    )
                    .child(
                        desktop_action_button("switch-wallet", "Switch wallet in desktop app")
                            .w_full()
                            .on_click(move |_, _, _| {
                                host_command("request_wallet_switch", &switch_request);
                            }),
                    );
            }
            screen = screen
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .min_w_0()
                        .child(note("Account"))
                        .child(self.render_connect_account(cx)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .min_w_0()
                        .child(note("Network"))
                        .child(match form {
                            Some(form) => Select::new(&form.chain)
                                .accessibility_label("Network")
                                .w_full()
                                .search_placeholder("Search networks")
                                .into_any_element(),
                            None => unavailable_select().into_any_element(),
                        }),
                )
                .child(
                    note(
                        "Shares the account address with this site. Signing and spending still need approval in the desktop app.",
                    )
                    .whitespace_normal(),
                );
        }
        screen.child(
            div()
                .mt_auto()
                .flex()
                .gap_2()
                .child(
                    app_button("reject-connect", "Reject")
                        .outline()
                        .flex_1()
                        .on_click(cx.listener(|this, _, _, cx| this.resolve_connect(None, cx))),
                )
                .child(if prompt.needs_unlock {
                    summon_desktop_button().flex_1().into_any_element()
                } else {
                    app_button("connect", "Connect")
                        .primary()
                        .flex_1()
                        .disabled(!ready)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let account = this
                                .connect_form
                                .as_ref()
                                .and_then(|form| form.account.read(cx).selected_value().cloned());
                            if let Some(account) = account {
                                this.resolve_connect(Some(account.as_str()), cx);
                            }
                        }))
                        .into_any_element()
                }),
        )
    }
}

/// The dapp's identity: the host reads at a glance, the canonical web origin defines the grant.
fn render_site(url: &str) -> Div {
    div()
        .flex()
        .gap_2p5()
        .items_start()
        .min_w_0()
        .child(
            div()
                .size(px(32.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .bg(rgb(theme::SURFACE_ELEVATED))
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(
                    Icon::new(IconName::Globe)
                        .small()
                        .text_color(rgb(theme::TEXT_MUTED)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w_0()
                .child(app_strong_text(site_host(url).to_owned()))
                .child(note(url.to_owned()).whitespace_normal()),
        )
}

/// Stands in for a select while the window update that builds the form is still pending.
fn unavailable_select() -> Div {
    div()
        .h(px(32.0))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
        .text_size(px(13.0))
        .text_color(rgb(theme::TEXT_PLACEHOLDER))
        .child("Unavailable")
}

fn desktop_action_button(id: &'static str, label: &'static str) -> Button {
    app_button(id, label).icon(Icon::empty().path("ui/icons/screen-share.svg"))
}

fn summon_desktop_button() -> Button {
    desktop_action_button("summon-desktop", "Open desktop app")
        .primary()
        .on_click(|_, _, _| host_command("summon_desktop", ""))
}

struct GatewayWindow {
    gateway: Entity<GatewayView>,
}

impl Render for GatewayWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(self.gateway.clone())
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

impl Render for GatewayView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let connecting = self.paired
            && matches!(self.status.as_str(), "paired" | "locked" | "unlocked")
            && !self.connect_prompts.is_empty();
        let body = if connecting {
            self.render_connect_request(cx)
        } else if self.paired {
            match self.status.as_str() {
                "locked" => self.render_locked(cx),
                "paired" | "unlocked" => self.render_connected(cx),
                _ => self.render_unreachable(cx),
            }
        } else {
            self.render_pairing(cx)
        };
        div()
            .id("gateway-scroll")
            .track_focus(&self.focus)
            .key_context("GatewayView")
            .on_action(cx.listener(|this, _: &public_view::Back, window, cx| {
                this.navigate_back(window, cx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .font_family(SANS)
            .text_size(theme::APP_TEXT_SIZE)
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_4()
                    .child(self.render_header(cx))
                    .children(self.render_draft_banner(cx))
                    .when(!self.view_notice.is_empty(), |this| {
                        this.child(note(self.view_notice.clone()))
                    })
                    .child(
                        div()
                            .id("gateway-content")
                            .flex_1()
                            .min_h_0()
                            .when(
                                self.draft_form.is_none(),
                                gpui::StatefulInteractiveElement::overflow_y_scroll,
                            )
                            .child(
                                div()
                                    .when(self.draft_form.is_some(), |this| this.h_full().min_h_0())
                                    .when(self.draft_form.is_none(), gpui::Styled::min_h_full)
                                    .flex()
                                    .flex_col()
                                    .child(body),
                            ),
                    ),
            )
    }
}
