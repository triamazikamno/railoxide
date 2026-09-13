//! The browser owns popover presentation; all network effects remain in the desktop.
use super::*;
use gpui_component::popover::Popover;
use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Duration};
use ui::network_status::{
    NetworkActivity, NetworkStatus, NetworkStatusIntent, NetworkStatusKind, NetworkStatusTrigger,
    TorExitIpQueryState, network_status_pill, network_status_popover_content,
    network_status_scroll,
};

pub(super) struct NetworkControl {
    status: Option<NetworkStatus>,
    activity: Option<NetworkActivity>,
    rate: Option<u64>,
    revision: String,
    view_id: String,
    open: bool,
    query: TorExitIpQueryState,
    confirming: bool,
    error: Option<Arc<str>>,
    sequence: u64,
    seen: BTreeMap<String, String>,
    focus: gpui::FocusHandle,
}
impl NetworkControl {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            status: None,
            activity: None,
            rate: None,
            revision: String::new(),
            view_id: String::new(),
            open: false,
            query: TorExitIpQueryState::Idle,
            confirming: false,
            error: None,
            sequence: 0,
            seen: BTreeMap::new(),
            focus: cx.focus_handle(),
        }
    }
    pub(super) fn retire(&mut self) {
        self.status = None;
        self.activity = None;
        self.rate = None;
        self.revision.clear();
        self.close();
    }
    fn close(&mut self) {
        self.open = false;
        self.view_id.clear();
        self.query = TorExitIpQueryState::Idle;
        self.confirming = false;
        self.error = None;
        self.seen.clear();
    }
    pub(super) fn sync(&mut self, snapshot: &JsValue) {
        let value = field(snapshot, "network_view");
        let kind = match text_field(&value, "status").as_str() {
            "tor_ready" => NetworkStatusKind::TorReady,
            "tor_reconnecting" => NetworkStatusKind::TorReconnecting,
            "tor_degraded" => NetworkStatusKind::TorDegraded,
            "proxy" => NetworkStatusKind::Proxy,
            "direct" => NetworkStatusKind::Direct,
            _ => {
                self.retire();
                return;
            }
        };
        if flag_field(snapshot, "locked") || !flag_field(snapshot, "network_control_supported") {
            self.retire();
            return;
        }
        let revision = text_field(&value, "context_revision");
        let popover = field(snapshot, "network_popover");
        let view_id = text_field(&popover, "view_id");
        if self.revision != revision {
            let keep_open = self.open && !view_id.is_empty();
            self.close();
            self.open = keep_open;
        } else if !self.view_id.is_empty() && self.view_id != view_id {
            self.close();
        }
        self.revision = revision;
        self.status = Some(NetworkStatus::new(
            kind,
            text_field(&value, "detail"),
            flag_field(&value, "runtime_warning"),
        ));
        self.rate = chain_id_field(&value, "download_rate");
        let activity = field(&value, "activity");
        self.activity = if activity.is_null() || activity.is_undefined() {
            None
        } else {
            let mut presentation = NetworkActivity::default();
            presentation.generation = chain_id_field(&activity, "generation").unwrap_or(0);
            presentation.session_duration = Duration::from_millis(
                chain_id_field(&activity, "session_duration_ms").unwrap_or(0),
            );
            presentation.downloaded_bytes =
                chain_id_field(&activity, "downloaded_bytes").unwrap_or(0);
            presentation.recent_connection_sample_count =
                chain_id_field(&activity, "recent_connection_sample_count").unwrap_or(0) as usize;
            presentation.recent_successful_sample_count =
                chain_id_field(&activity, "recent_successful_sample_count").unwrap_or(0) as usize;
            presentation.successful_connections =
                chain_id_field(&activity, "successful_connections").unwrap_or(0);
            presentation.failed_connections =
                chain_id_field(&activity, "failed_connections").unwrap_or(0);
            presentation.median_setup_duration =
                chain_id_field(&activity, "median_setup_duration_ms").map(Duration::from_millis);
            presentation.last_activity_age =
                chain_id_field(&activity, "last_activity_age_ms").map(Duration::from_millis);
            Some(presentation)
        };
        if !self.open || view_id.is_empty() {
            return;
        }
        self.view_id = view_id;
        for result in js_sys::Array::from(&field(&popover, "results")).iter() {
            let request_id = text_field(&result, "request_id");
            let operation = text_field(&result, "operation");
            if request_id.is_empty() || self.seen.get(&operation) == Some(&request_id) {
                continue;
            }
            self.seen.insert(operation.clone(), request_id);
            let outcome = field(&result, "outcome");
            match text_field(&outcome, "status").as_str() {
                "exit_ip"
                    if operation == "query_exit_ip"
                        && matches!(self.query, TorExitIpQueryState::Querying) =>
                {
                    if let Ok(ip) = text_field(&outcome, "ip").parse::<IpAddr>() {
                        self.query = TorExitIpQueryState::Success(ip);
                    }
                }
                "failed" => {
                    let error = Arc::from(text_field(&outcome, "error"));
                    if operation == "query_exit_ip"
                        && matches!(self.query, TorExitIpQueryState::Querying)
                    {
                        self.query = TorExitIpQueryState::Error(error);
                    } else {
                        self.error = Some(error);
                        self.confirming = false;
                    }
                }
                _ => {}
            }
        }
    }
}

impl GatewayView {
    fn set_network_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if open && self.network.status.is_some() {
            self.network.open = true;
            host_command(
                "network",
                &serde_json::json!({"action":"open", "context_revision":self.network.revision})
                    .to_string(),
            );
        } else {
            host_command("network", &serde_json::json!({"action":"close", "context_revision":self.network.revision, "view_id":self.network.view_id}).to_string());
            self.network.close();
            self.network.focus.focus(window, cx);
        }
        cx.notify();
    }
    fn network_intent(&mut self, intent: NetworkStatusIntent, cx: &mut Context<'_, Self>) {
        if !self.network.open || self.network.view_id.is_empty() || self.network.status.is_none() {
            return;
        }
        self.network.error = None;
        let operation = match intent {
            NetworkStatusIntent::BeginReset => {
                self.network.query = TorExitIpQueryState::Idle;
                self.network.confirming = true;
                host_command("network", &serde_json::json!({"action":"cancel_query", "context_revision":self.network.revision, "view_id":self.network.view_id}).to_string());
                cx.notify();
                return;
            }
            NetworkStatusIntent::CancelReset => {
                self.network.confirming = false;
                cx.notify();
                return;
            }
            NetworkStatusIntent::QuitAndReset if !self.network.confirming => return,
            NetworkStatusIntent::QuitAndReset => "quit_and_reset",
            NetworkStatusIntent::NewTorSession => "new_tor_session",
            NetworkStatusIntent::QueryExitIp => {
                if matches!(self.network.query, TorExitIpQueryState::Querying) {
                    return;
                }
                self.network.query = TorExitIpQueryState::Querying;
                "query_exit_ip"
            }
        };
        self.network.sequence += 1;
        host_command("network", &serde_json::json!({"action":"run", "context_revision":self.network.revision, "view_id":self.network.view_id,
            "request_id":format!("{}:{}", self.network.view_id, self.network.sequence), "operation":operation}).to_string());
        cx.notify();
    }
    pub(super) fn render_network_button(&self, cx: &Context<'_, Self>) -> AnyElement {
        let Some(status) = self.network.status.clone() else {
            return div().into_any_element();
        };
        let root = cx.entity();
        let content_root = root.clone();
        let activity = self.network.activity.clone();
        let rate = self.network.rate;
        let error = self.network.error.clone();
        let query = self.network.query.clone();
        let confirming = self.network.confirming;
        let revision = self.network.revision.clone();
        let view_id = self.network.view_id.clone();
        let trigger = network_status_pill(
            "gateway-network-trigger",
            true,
            &status,
            activity.as_ref(),
            rate,
            px(0.0),
        );
        Popover::new("gateway-network-popover")
            .anchor(Anchor::TopRight)
            .p_0()
            .open(self.network.open)
            .trigger(NetworkStatusTrigger::new(
                trigger,
                &self.network.focus,
                &status,
            ))
            .on_open_change(move |open, window, cx| {
                root.update(cx, |root, cx| root.set_network_open(*open, window, cx));
            })
            .content(move |_, window, _| {
                let owner = content_root.clone();
                let copy_owner = owner.clone();
                let copy_revision = revision.clone();
                let copy_view_id = view_id.clone();
                let intent_revision = revision.clone();
                let intent_view_id = view_id.clone();
                let copy_query = query.clone();
                let copy = matches!(query, TorExitIpQueryState::Success(_)).then(|| {
                    app_button_base("gateway-network-copy")
                        .icon(IconName::Copy)
                        .ghost()
                        .xsmall()
                        .accessibility_label("Copy exit IP")
                        .tooltip("Copy exit IP")
                        .on_click(move |_, window, cx| {
                            let root = copy_owner.read(cx);
                            if let TorExitIpQueryState::Success(ip) = copy_query {
                                let address = ip.to_string();
                                if root.network.open
                                    && root.network.revision == copy_revision
                                    && root.network.view_id == copy_view_id
                                    && root.network.query == copy_query
                                    && host_can_copy_network(
                                        &chain_value(root.generation.unwrap_or(0)),
                                        &root.network.revision,
                                        &root.network.view_id,
                                        &address,
                                    )
                                {
                                    ui::clipboard::copy_to_clipboard_with_toast(
                                        address, window, cx,
                                    );
                                }
                            }
                        })
                        .into_any_element()
                });
                network_status_scroll(
                    network_status_popover_content(
                        &status,
                        error.clone(),
                        query.clone(),
                        confirming,
                        activity.as_ref(),
                        rate,
                        move |intent, _, cx| {
                            owner.update(cx, |owner, cx| {
                                if owner.network.revision == intent_revision
                                    && owner.network.view_id == intent_view_id
                                {
                                    owner.network_intent(intent, cx);
                                }
                            });
                        },
                        copy,
                    ),
                    window,
                )
            })
            .into_any_element()
    }
}
